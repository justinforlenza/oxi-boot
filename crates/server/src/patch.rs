//! Decides how a Linux ISO is patched so that it can find itself again after
//! the kernel starts: every initrd on it gets the helper as its first
//! program. Nothing is written to disk; the result is a view of the ISO.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::sync::OnceCell;

use crate::initrd::{self, Added};
use crate::iso::{self, Piece, Record, View};

/// Debian's installer only looks for its disc on drives marked as a CD.
const INSTALLER_UDEV_RULE: &str = "KERNEL==\"nbd0\", ENV{ID_CDROM}=\"1\", ENV{ID_TYPE}=\"cd\"\n";

fn is_initrd(name: &str) -> bool {
    name.contains("initrd") || name.contains("initramfs")
}

fn is_kernel(name: &str) -> bool {
    name.starts_with("vmlinuz") || name.starts_with("bzimage") || name == "linux"
}

/// Files a boot loader or the firmware may read, which must not be laid over.
/// shortcut: guessed from names and sizes. Read the El Torito boot catalogue
/// and the boot loader's config if a disc ever breaks this guess.
fn boot_loader_may_read(record: &Record) -> bool {
    let name = record.name();
    is_kernel(&name) || name.ends_with(".efi") || (record.len < 64 << 20 && (name.ends_with(".img") || name.ends_with(".bin")))
}

/// Reads one file of the ISO.
struct Extent<'a> {
    file: &'a File,
    at: u64,
    end: u64,
}

impl Read for Extent<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min((self.end - self.at) as usize);
        let n = self.file.read_at(&mut buf[..n], self.at)?;
        self.at += n as u64;
        Ok(n)
    }
}

fn extent<'a>(file: &'a File, record: &Record) -> Extent<'a> {
    Extent { file, at: record.offset(), end: record.offset() + record.len as u64 }
}

/// The network and NBD drivers for a Debian-style installer, whose initrd
/// ships none: they are on the disc as installer packages (udebs).
fn installer_modules(file: &File, records: &[Record], kernel: &str, lib: &str) -> io::Result<Vec<Added>> {
    let (mut added, mut seen) = (Vec::new(), HashSet::new());
    for record in records {
        let name = record.name();
        let wanted = ["nic-modules-", "nic-shared-modules-", "nbd-modules-"].iter().any(|p| name.starts_with(p));
        if !wanted || !name.ends_with(".udeb") || !name.contains(&kernel.to_ascii_lowercase()) || !seen.insert(record.lba) {
            continue;
        }
        // A udeb is an ar archive holding data.tar.<compression>.
        let mut package = ar::Archive::new(extent(file, record));
        while let Some(member) = package.next_entry() {
            let member = member?;
            let id = String::from_utf8_lossy(member.header().identifier()).into_owned();
            let data: Box<dyn Read> = match id.as_str() {
                "data.tar.xz" => Box::new(liblzma::read::XzDecoder::new(member)),
                "data.tar.gz" => Box::new(flate2::read::GzDecoder::new(member)),
                "data.tar.zst" => Box::new(zstd::Decoder::new(member)?),
                "data.tar" => Box::new(member),
                _ => continue,
            };
            for entry in tar::Archive::new(data).entries()? {
                let mut entry = entry?;
                let path = entry.path()?.to_string_lossy().trim_start_matches("./").to_string();
                let path = path.strip_prefix("usr/").unwrap_or(&path);
                if entry.header().entry_type().is_file() && path.starts_with("lib/modules/") {
                    let mut content = Vec::new();
                    entry.read_to_end(&mut content)?;
                    added.push(Added::file(&format!("{lib}{}", &path[3..]), 0o644, content));
                }
            }
        }
    }
    Ok(added)
}

/// The view of `file` to boot from. `helper` is the helper program and
/// `conf` what it needs to find the server and the untouched ISO.
pub fn plan(file: &File, helper: &[u8], conf: &str) -> io::Result<View> {
    let records = iso::files(file)?;
    let (mut moved, mut seen) = (Vec::new(), HashSet::new());
    // An initrd is named like one, or sits beside a kernel under any name
    // (SystemRescue's is sysresccd.img). Whether it really is one, the scan tells.
    let folder = |r: &Record| r.path.rsplit_once('/').map_or(String::new(), |(dir, _)| dir.to_string());
    let kernel_folders: HashSet<String> = records.iter().filter(|r| is_kernel(&r.name())).map(folder).collect();
    for record in &records {
        let name = record.name();
        let package = [".deb", ".udeb", ".rpm"].iter().any(|ext| name.ends_with(ext));
        let candidate = is_initrd(&name) || (kernel_folders.contains(&folder(record)) && !is_kernel(&name));
        if !candidate || package || record.len == 0 || !seen.insert(record.lba) {
            continue;
        }
        let scan = match initrd::scan(extent(file, record)) {
            Ok(scan) if scan.init.is_some() => scan,
            _ => {
                if is_initrd(&name) {
                    eprintln!("patch: {} left alone: not an initrd with an /init", record.path);
                }
                continue;
            }
        };
        let (mode, original) = scan.init.clone().expect("checked above");
        // The helper replaces /init and finds the original beside it.
        let mut entries = vec![
            Added::file("init", 0o755, helper.to_vec()),
            Added { path: "init.oxi-orig".into(), mode, content: original },
            Added::file("oxi-boot.conf", 0o644, conf.as_bytes().to_vec()),
        ];
        let mut note = "";
        if !scan.has_nbd {
            // Added files must follow the initrd's own /lib symlink, or they replace it.
            let lib = if scan.lib_is_link { "usr/lib" } else { "lib" };
            let modules = installer_modules(file, &records, scan.kernel.as_deref().unwrap_or("\0"), lib)?;
            if modules.is_empty() {
                note = " (no NBD driver in it or on the disc: the driver must be built into its kernel)";
            } else {
                note = " (installer: network and NBD drivers added from the disc)";
                entries.extend(modules);
                entries.push(Added::file("etc/udev/rules.d/00-oxi-boot.rules", 0o644, INSTALLER_UDEV_RULE.into()));
                // Tells the helper to index the added modules before loading any.
                entries.push(Added::file("oxi-boot.depmod", 0o644, Vec::new()));
            }
        }
        eprintln!("patch: {} gets the helper{note}", record.path);
        // An archive must start on a 4-byte boundary.
        let mut appended = vec![0; record.len.next_multiple_of(4) as usize - record.len as usize];
        appended.extend(initrd::archive(&entries)?);
        moved.push((record.lba, vec![Piece::Iso { offset: record.offset(), len: record.len as u64 }, Piece::Bytes(appended)]));
    }
    if moved.is_empty() {
        return View::unpatched(file);
    }
    // On BIOS a kernel that lies past 4 GiB cannot be read either: move it down, unchanged.
    for record in records.iter().filter(|r| is_kernel(&r.name()) && r.offset() + r.len as u64 > 1 << 32) {
        if seen.insert(record.lba) {
            eprintln!("patch: {} moved below 4 GiB", record.path);
            moved.push((record.lba, vec![Piece::Iso { offset: record.offset(), len: record.len as u64 }]));
        }
    }
    View::replace(file, &records, moved, |r| !boot_loader_may_read(r))
}

/// An ISO opened for serving.
pub struct Patched {
    pub file: File,
    pub view: View,
}

type Slot = (SystemTime, u64, Arc<OnceCell<Arc<Patched>>>);

/// The patched views, worked out once per ISO and kept until the file changes.
pub struct Patches {
    iso_dir: PathBuf,
    /// The helper program; None if it is not installed, and then nothing is patched.
    helper: Option<Vec<u8>>,
    /// The server as the helper reaches it: "address:port".
    server: String,
    slots: Mutex<HashMap<String, Slot>>,
}

impl Patches {
    pub fn new(iso_dir: PathBuf, helper: Option<Vec<u8>>, server: String) -> Patches {
        Patches { iso_dir, helper, server, slots: Mutex::new(HashMap::new()) }
    }

    /// The view of the ISO at `path` under the ISO folder. `url_path` is the
    /// same path as it appears in the ISO's own URL. The first call for an
    /// ISO unpacks its initrds, which can take a while; later calls wait for
    /// that or return at once.
    pub async fn get(&self, path: &str, url_path: &str) -> io::Result<Arc<Patched>> {
        let full = self.iso_dir.join(path);
        let meta = tokio::fs::metadata(&full).await?;
        let (modified, len) = (meta.modified()?, meta.len());
        let cell = {
            let mut slots = self.slots.lock().unwrap();
            let slot = slots.entry(path.to_string()).or_insert_with(|| (modified, len, Arc::default()));
            if (slot.0, slot.1) != (modified, len) {
                *slot = (modified, len, Arc::default()); // the file was replaced
            }
            slot.2.clone()
        };
        let (helper, name) = (self.helper.clone(), path.to_string());
        let conf = format!("{}\n/iso/{url_path}\n", self.server);
        let patched = cell
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || {
                    let file = File::open(full)?;
                    let view = match helper {
                        Some(helper) => plan(&file, &helper, &conf).unwrap_or_else(|e| {
                            eprintln!("patch: {name} served as-is: {e}");
                            View::unpatched(&file).expect("file was just opened")
                        }),
                        None => View::unpatched(&file)?,
                    };
                    eprintln!("patch: {name} is ready ({})", if view.is_patched() { "patched" } else { "as-is" });
                    Ok::<_, io::Error>(Arc::new(Patched { file, view }))
                })
                .await
                .map_err(io::Error::other)?
            })
            .await?;
        Ok(patched.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initrd_on_the_disc_gets_the_helper() {
        // The test image's initrd is not a real one, so first check it is left alone...
        let file = iso::tests::image("patch-junk");
        assert!(!plan(&file, b"helper", "conf").unwrap().is_patched());

        // ...then lay a real initrd over it and plan again.
        let real = initrd::archive(&[Added::file("init", 0o755, b"#!/bin/sh\n".to_vec())]).unwrap();
        let file = iso::tests::image_with_initrd("patch-real", &real);
        let view = plan(&file, b"the helper", "10.0.0.1:80\n/iso/a.iso\n").unwrap();
        assert!(view.is_patched());

        // Read the initrd back through the view, as a boot loader would.
        let records = iso::tests::files_in_view(&view, &file);
        let initrd = records.iter().find(|r| r.path == "BOOT/INITRD.IMG").unwrap();
        let bytes = view.read(&file, initrd.offset(), initrd.len as usize).unwrap();
        assert_eq!(&bytes[..real.len()], real);
        // The kernel unpacks archives in order, so the last /init wins: ours.
        let scan = initrd::scan(&bytes[..]).unwrap();
        assert_eq!(scan.init, Some((0o100755, b"the helper".to_vec())));
    }
}
