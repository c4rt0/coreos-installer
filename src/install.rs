// Copyright 2019 CoreOS, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use error_chain::{bail, ensure, ChainedError};
use nix::mount;
use std::fs::{copy as fscopy, create_dir_all, read_dir, File, OpenOptions};
use std::io::{copy, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use crate::blockdev::*;
use crate::cmdline::*;
use crate::download::*;
use crate::errors::*;
use crate::source::*;

/// Integrity verification hash for an Ignition config.
#[derive(Debug)]
pub enum IgnitionHash {
    /// SHA-512 digest.
    Sha512(Vec<u8>),
}

impl IgnitionHash {
    /// Try to parse an hash-digest argument.
    ///
    /// This expects an input value following the `ignition.config.verification.hash`
    /// spec, i.e. `<type>-<value>` format.
    pub fn try_parse(input: &str) -> Result<Self> {
        let parts: Vec<_> = input.splitn(2, '-').collect();
        if parts.len() != 2 {
            bail!("failed to detect hash-type and digest in '{}'", input);
        }
        let (hash_kind, hex_digest) = (parts[0], parts[1]);

        let hash = match hash_kind {
            "sha512" => {
                let digest = hex::decode(hex_digest).chain_err(|| "decoding hex digest")?;
                ensure!(
                    digest.len().saturating_mul(8) == 512,
                    "wrong digest length ({})",
                    digest.len().saturating_mul(8)
                );
                IgnitionHash::Sha512(digest)
            }
            x => bail!("unknown hash type '{}'", x),
        };

        Ok(hash)
    }

    /// Digest and validate input data.
    pub fn validate(&self, input: &mut impl Read) -> Result<()> {
        use sha2::digest::Digest;

        let (mut hasher, digest) = match self {
            IgnitionHash::Sha512(val) => (sha2::Sha512::new(), val),
        };
        copy(input, &mut hasher).chain_err(|| "copying input to hasher")?;
        let computed = hasher.finalize();

        if computed.as_slice() != digest.as_slice() {
            bail!(
                "hash mismatch, computed '{}' but expected '{}'",
                hex::encode(computed),
                hex::encode(digest),
            );
        }

        Ok(())
    }
}

pub fn install(config: &InstallConfig) -> Result<()> {
    // set up image source
    // we only support installing from a single artifact
    let mut sources = config.location.sources()?;
    let mut source = sources.pop().chain_err(|| "no artifacts found")?;
    if !sources.is_empty() {
        bail!("found multiple artifacts");
    }
    if source.signature.is_none() && config.location.require_signature() {
        if config.insecure {
            eprintln!("Signature not found; skipping verification as requested");
        } else {
            bail!("--insecure not specified and signature not found");
        }
    }

    // open output; ensure it's a block device and we have exclusive access
    let mut dest = OpenOptions::new()
        .write(true)
        .open(&config.device)
        .chain_err(|| format!("opening {}", &config.device))?;
    if !dest
        .metadata()
        .chain_err(|| format!("getting metadata for {}", &config.device))?
        .file_type()
        .is_block_device()
    {
        bail!("{} is not a block device", &config.device);
    }
    if let Err(e) = reread_partition_table(&mut dest) {
        report_busy_partitions(&config.device)?;
        Err(e).chain_err(|| format!("checking for exclusive access to {}", &config.device))?;
    }

    // copy and postprocess disk image
    // On failure, clear and reread the partition table to prevent the disk
    // from accidentally being used.
    if let Err(err) = write_disk(&config, &mut source, &mut dest) {
        // log the error so the details aren't dropped if we encounter
        // another error during cleanup
        eprint!("{}", ChainedError::display_chain(&err));

        // clean up
        if config.preserve_on_error {
            eprintln!("Preserving partition table as requested");
        } else {
            clear_partition_table(&mut dest)?;
        }

        // return a generic error so our exit status is right
        bail!("install failed");
    }

    eprintln!("Install complete.");
    Ok(())
}

fn report_busy_partitions(device: &str) -> Result<()> {
    let mut parts = Disk::new(device).get_busy_partitions()?;
    parts.sort_unstable_by_key(|p| p.path.to_string());
    if parts.is_empty() {
        return Ok(());
    }
    eprintln!("Partitions in use on {}:", device);
    for part in parts {
        if let Some(mountpoint) = part.mountpoint.as_ref() {
            eprintln!("    {} mounted on {}", part.path, mountpoint);
        }
        if part.swap {
            eprintln!("    {} is swap device", part.path);
        }
        for holder in part.get_holders()? {
            eprintln!("    {} in use by {}", part.path, holder);
        }
    }
    Ok(())
}

/// Copy the image source to the target disk and do all post-processing.
/// If this function fails, the caller should wipe the partition table
/// to ensure the user doesn't boot from a partially-written disk.
fn write_disk(config: &InstallConfig, source: &mut ImageSource, dest: &mut File) -> Result<()> {
    // Get sector size of destination, for comparing with image
    let sector_size = get_sector_size(dest)?;

    // copy the image
    write_image(source, dest, true, Some(sector_size))?;
    reread_partition_table(dest)?;
    udev_settle()?;

    // postprocess
    if config.ignition.is_some()
        || config.firstboot_kargs.is_some()
        || config.append_kargs.is_some()
        || config.delete_kargs.is_some()
        || config.platform.is_some()
        || config.network_config.is_some()
    {
        let mount =
            Disk::new(&config.device).mount_partition_by_label("boot", mount::MsFlags::empty())?;
        if let Some(ignition) = config.ignition.as_ref() {
            write_ignition(mount.mountpoint(), &config.ignition_hash, ignition)
                .chain_err(|| "writing Ignition configuration")?;
        }
        if let Some(firstboot_kargs) = config.firstboot_kargs.as_ref() {
            write_firstboot_kargs(mount.mountpoint(), firstboot_kargs)
                .chain_err(|| "writing firstboot kargs")?;
        }
        if config.append_kargs.is_some() || config.delete_kargs.is_some() {
            eprintln!("Modifying kernel arguments");

            edit_bls_entries(mount.mountpoint(), |orig_contents: &str| {
                bls_entry_delete_and_append_kargs(
                    orig_contents,
                    config.delete_kargs.as_ref(),
                    config.append_kargs.as_ref(),
                )
            })
            .chain_err(|| "deleting and appending kargs")?;
        }
        if let Some(platform) = config.platform.as_ref() {
            write_platform(mount.mountpoint(), platform).chain_err(|| "writing platform ID")?;
        }
        if let Some(network_config) = config.network_config.as_ref() {
            copy_network_config(mount.mountpoint(), network_config)?;
        }
    }

    Ok(())
}

/// Write the Ignition config.
fn write_ignition(
    mountpoint: &Path,
    digest_in: &Option<IgnitionHash>,
    mut config_in: &File,
) -> Result<()> {
    eprintln!("Writing Ignition config");

    // Verify configuration digest, if any.
    if let Some(ref digest) = digest_in {
        digest
            .validate(&mut config_in)
            .chain_err(|| "failed to validate Ignition configuration digest")?;
        config_in
            .seek(SeekFrom::Start(0))
            .chain_err(|| "rewinding Ignition configuration file")?;
    };

    // make parent directory
    let mut config_dest = mountpoint.to_path_buf();
    config_dest.push("ignition");
    create_dir_all(&config_dest).chain_err(|| "creating Ignition config directory")?;

    // do the copy
    config_dest.push("config.ign");
    let mut config_out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_dest)
        .chain_err(|| {
            format!(
                "opening destination Ignition config {}",
                config_dest.display()
            )
        })?;
    copy(&mut config_in, &mut config_out).chain_err(|| "writing Ignition config")?;

    Ok(())
}

/// Write first-boot kernel arguments.
fn write_firstboot_kargs(mountpoint: &Path, args: &str) -> Result<()> {
    eprintln!("Writing first-boot kernel arguments");

    // write the arguments
    let mut config_dest = mountpoint.to_path_buf();
    config_dest.push("ignition.firstboot");
    // if the file doesn't already exist, fail, since our assumptions
    // are wrong
    let mut config_out = OpenOptions::new()
        .append(true)
        .open(&config_dest)
        .chain_err(|| format!("opening first-boot file {}", config_dest.display()))?;
    let contents = format!("set ignition_network_kcmdline=\"{}\"\n", args);
    config_out
        .write_all(contents.as_bytes())
        .chain_err(|| "writing first-boot kernel arguments")?;

    Ok(())
}

// This is split out so that we can unit test it.
fn bls_entry_delete_and_append_kargs(
    orig_contents: &str,
    delete_args: Option<&Vec<String>>,
    append_args: Option<&Vec<String>>,
) -> Result<String> {
    let mut new_contents = String::with_capacity(orig_contents.len());
    let mut found_options = false;
    for line in orig_contents.lines() {
        if !line.starts_with("options ") {
            new_contents.push_str(line.trim_end());
        } else if found_options {
            bail!("Multiple 'options' lines found");
        } else {
            // XXX: Need a proper parser here and share it with afterburn. The approach we use here
            // is to just do a dumb substring search and replace. This is naive (e.g. doesn't
            // handle occurrences in quoted args) but will work for now (one thing that saves us is
            // that we're acting on our baked configs, which have straight-forward kargs).
            new_contents.push_str("options ");
            let mut line: String = add_whitespaces(&line["options ".len()..]);
            if let Some(args) = delete_args {
                for arg in args {
                    let arg = add_whitespaces(&arg);
                    line = line.replace(&arg, " ");
                }
            }
            new_contents.push_str(line.trim_start().trim_end());
            if let Some(args) = append_args {
                for arg in args {
                    new_contents.push(' ');
                    new_contents.push_str(&arg);
                }
            }
            found_options = true;
        }
        new_contents.push('\n');
    }
    if !found_options {
        bail!("Couldn't locate 'options' line");
    }
    Ok(new_contents)
}

fn add_whitespaces(s: &str) -> String {
    let mut r: String = s.into();
    r.insert(0, ' ');
    r.push(' ');
    r
}

/// Override the platform ID.
fn write_platform(mountpoint: &Path, platform: &str) -> Result<()> {
    // early return if setting the platform to the default value, since
    // otherwise we'll think we failed to set it
    if platform == "metal" {
        return Ok(());
    }

    eprintln!("Setting platform to {}", platform);
    edit_bls_entries(mountpoint, |orig_contents: &str| {
        bls_entry_write_platform(orig_contents, platform)
    })?;

    Ok(())
}

/// Modifies the BLS config, only changing the `ignition.platform.id`. This assumes that we will
/// only install from metal images and that the bootloader configs will always set
/// ignition.platform.id.  Fail if those assumptions change.  This is deliberately simplistic.
fn bls_entry_write_platform(orig_contents: &str, platform: &str) -> Result<String> {
    let new_contents = orig_contents.replace(
        "ignition.platform.id=metal",
        &format!("ignition.platform.id={}", platform),
    );
    if orig_contents == new_contents {
        bail!("Couldn't locate platform ID");
    }
    Ok(new_contents)
}

/// Apply a transforming function on each BLS entry found.
fn edit_bls_entries(mountpoint: &Path, f: impl Fn(&str) -> Result<String>) -> Result<()> {
    // walk /boot/loader/entries/*.conf
    let mut config_path = mountpoint.to_path_buf();
    config_path.push("loader/entries");
    for entry in read_dir(&config_path)
        .chain_err(|| format!("reading directory {}", config_path.display()))?
    {
        let path = entry
            .chain_err(|| format!("reading directory {}", config_path.display()))?
            .path();
        if path.extension().unwrap_or_default() == "conf" {
            // slurp in the file
            let mut config = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .chain_err(|| format!("opening bootloader config {}", path.display()))?;
            let orig_contents = {
                let mut s = String::new();
                config
                    .read_to_string(&mut s)
                    .chain_err(|| format!("reading {}", path.display()))?;
                s
            };

            let new_contents =
                f(&orig_contents).chain_err(|| format!("modifying {}", path.display()))?;

            // write out the modified data
            config
                .seek(SeekFrom::Start(0))
                .chain_err(|| format!("seeking {}", path.display()))?;
            config
                .set_len(0)
                .chain_err(|| format!("truncating {}", path.display()))?;
            config
                .write(new_contents.as_bytes())
                .chain_err(|| format!("writing {}", path.display()))?;
        }
    }

    Ok(())
}

/// Copy networking config if asked to do so
fn copy_network_config(mountpoint: &Path, net_config_src: &str) -> Result<()> {
    eprintln!("Copying networking configuration from {}", net_config_src);

    // get the path to the destination directory
    let net_config_dest = mountpoint.join("coreos-firstboot-network");

    // make the directory if it doesn't exist
    create_dir_all(&net_config_dest).chain_err(|| {
        format!(
            "creating destination networking config directory {}",
            net_config_dest.display()
        )
    })?;

    // copy files from source to destination directories
    for entry in
        read_dir(&net_config_src).chain_err(|| format!("reading directory {}", net_config_src))?
    {
        let entry = entry.chain_err(|| format!("reading directory {}", net_config_src))?;
        let srcpath = entry.path();
        let destpath = net_config_dest.join(entry.file_name());
        if srcpath.is_file() {
            eprintln!("Copying {} to installed system", srcpath.display());
            fscopy(&srcpath, &destpath).chain_err(|| "Copying networking config")?;
        }
    }

    Ok(())
}

/// Clear the partition table.  For use after a failure.
fn clear_partition_table(dest: &mut File) -> Result<()> {
    eprintln!("Clearing partition table");
    dest.seek(SeekFrom::Start(0))
        .chain_err(|| "seeking to start of disk")?;
    let zeroes: [u8; 1024 * 1024] = [0; 1024 * 1024];
    dest.write_all(&zeroes)
        .chain_err(|| "clearing partition table")?;
    dest.flush()
        .chain_err(|| "flushing partition table to disk")?;
    dest.sync_all()
        .chain_err(|| "syncing partition table to disk")?;
    reread_partition_table(dest)?;
    udev_settle()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ignition_hash_cli_parse() {
        let err_cases = vec!["", "foo-bar", "-bar", "sha512", "sha512-", "sha512-00"];
        for arg in err_cases {
            IgnitionHash::try_parse(arg).expect_err(&format!("input: {}", arg));
        }

        let null_digest = "sha512-cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
        IgnitionHash::try_parse(null_digest).unwrap();
    }

    #[test]
    fn test_ignition_hash_validate() {
        let input = vec![b'a', b'b', b'c'];
        let hash_arg = "sha512-ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
        let hasher = IgnitionHash::try_parse(&hash_arg).unwrap();
        let mut rd = std::io::Cursor::new(input);
        hasher.validate(&mut rd).unwrap();
    }

    #[test]
    fn test_platform_id() {
        let orig_content = "options ignition.platform.id=metal foo bar";
        let new_content = bls_entry_write_platform(orig_content, "openstack").unwrap();
        assert_eq!(
            new_content,
            "options ignition.platform.id=openstack foo bar"
        );

        let orig_content = "options foo ignition.platform.id=metal bar";
        let new_content = bls_entry_write_platform(orig_content, "openstack").unwrap();
        assert_eq!(
            new_content,
            "options foo ignition.platform.id=openstack bar"
        );

        let orig_content = "options foo bar ignition.platform.id=metal";
        let new_content = bls_entry_write_platform(orig_content, "openstack").unwrap();
        assert_eq!(
            new_content,
            "options foo bar ignition.platform.id=openstack"
        );
    }

    #[test]
    fn test_options_edit() {
        let orig_content = "options foo bar foobar";

        let delete_kargs = vec!["foo".into()];
        let new_content =
            bls_entry_delete_and_append_kargs(orig_content, Some(&delete_kargs), None).unwrap();
        assert_eq!(new_content, "options bar foobar\n");

        let delete_kargs = vec!["bar".into()];
        let new_content =
            bls_entry_delete_and_append_kargs(orig_content, Some(&delete_kargs), None).unwrap();
        assert_eq!(new_content, "options foo foobar\n");

        let delete_kargs = vec!["foobar".into()];
        let new_content =
            bls_entry_delete_and_append_kargs(orig_content, Some(&delete_kargs), None).unwrap();
        assert_eq!(new_content, "options foo bar\n");

        let delete_kargs = vec!["bar".into(), "foo".into()];
        let new_content =
            bls_entry_delete_and_append_kargs(orig_content, Some(&delete_kargs), None).unwrap();
        assert_eq!(new_content, "options foobar\n");

        let orig_content = "options foo=val bar baz=val";

        let delete_kargs = vec!["foo=val".into()];
        let new_content =
            bls_entry_delete_and_append_kargs(orig_content, Some(&delete_kargs), None).unwrap();
        assert_eq!(new_content, "options bar baz=val\n");

        let delete_kargs = vec!["baz=val".into()];
        let new_content =
            bls_entry_delete_and_append_kargs(orig_content, Some(&delete_kargs), None).unwrap();
        assert_eq!(new_content, "options foo=val bar\n");

        let orig_content =
            "options foo mitigations=auto,nosmt console=tty0 bar console=ttyS0,115200n8 baz";

        let delete_kargs = vec![
            "mitigations=auto,nosmt".into(),
            "console=ttyS0,115200n8".into(),
        ];
        let append_kargs = vec!["console=ttyS1,115200n8".into()];
        let new_content = bls_entry_delete_and_append_kargs(
            orig_content,
            Some(&delete_kargs),
            Some(&append_kargs),
        )
        .unwrap();
        assert_eq!(
            new_content,
            "options foo console=tty0 bar baz console=ttyS1,115200n8\n"
        );
    }
}
