//! Early-boot images (initrds): what is inside one, and the archive we
//! append to it so that the helper runs first.
//!
//! An initrd is a run of cpio archives. Any of them may be compressed, and
//! the kernel unpacks them in order into one file system, later files
//! replacing earlier ones. So appending an archive is enough to add or
//! replace files; nothing has to be repacked.

use std::io::{self, BufRead, BufReader, Read, Write};

use cpio::newc;

/// What the patch needs to know about an initrd.
#[derive(Debug, Default, PartialEq)]
pub struct Scan {
    /// The first program as (mode, content); for a symlink the content is its target.
    pub init: Option<(u32, Vec<u8>)>,
    /// Whether it ships the network block device driver as a module.
    pub has_nbd: bool,
    /// Whether `/lib` is a symlink (to `usr/lib`), which added files must follow.
    pub lib_is_link: bool,
    /// The kernel version its modules are for.
    pub kernel: Option<String>,
    /// The machine type in the header of the first program found (183 is ARM64).
    pub machine: Option<u16>,
}

const SYMLINK: u32 = 0o120000;
const FILE_TYPE: u32 = 0o170000;

pub fn scan(reader: impl Read) -> io::Result<Scan> {
    let mut scan = Scan::default();
    scan_into(&mut BufReader::with_capacity(1 << 16, reader), &mut scan)?;
    Ok(scan)
}

// `dyn`: each compression layer wraps the reader, so a generic one would never end.
fn scan_into(reader: &mut dyn BufRead, scan: &mut Scan) -> io::Result<()> {
    loop {
        // Archives are padded with zeros.
        let magic = loop {
            let buf = reader.fill_buf()?;
            match buf.iter().position(|b| *b != 0) {
                _ if buf.is_empty() => return Ok(()),
                Some(0) => break buf[..buf.len().min(6)].to_vec(),
                skip => {
                    let n = skip.unwrap_or(buf.len());
                    reader.consume(n);
                }
            }
        };
        match magic.as_slice() {
            [b'0', b'7', b'0', b'7', ..] => loop {
                let mut entry = newc::Reader::new(&mut *reader)?;
                let (name, mode) = (entry.entry().name().trim_start_matches("./").trim_start_matches('/').to_string(), entry.entry().mode());
                if entry.entry().is_trailer() {
                    entry.finish()?;
                    break;
                }
                // The start of each file, until one is a program and tells the machine type.
                let mut content = Vec::new();
                if scan.machine.is_none() && mode & FILE_TYPE == 0o100000 {
                    (&mut entry).take(20).read_to_end(&mut content)?;
                    if content.len() == 20 && content.starts_with(b"\x7fELF") {
                        scan.machine = Some(u16::from_le_bytes([content[18], content[19]]));
                    }
                }
                match name.as_str() {
                    "init" => {
                        entry.read_to_end(&mut content)?;
                        scan.init = Some((mode, content));
                    }
                    "lib" => scan.lib_is_link = mode & FILE_TYPE == SYMLINK,
                    _ => {}
                }
                if let Some(rest) = name.strip_prefix("usr/").unwrap_or(&name).strip_prefix("lib/modules/") {
                    if let Some((version, _)) = rest.split_once('/') {
                        scan.kernel.get_or_insert_with(|| version.to_string());
                    }
                    scan.has_nbd |= rest.rsplit('/').next().is_some_and(|file| file == "nbd.ko" || file.starts_with("nbd.ko."));
                }
                entry.finish()?;
            },
            // The compressed part is taken to be the last: its decoder reads to the end.
            [0x28, 0xb5, 0x2f, 0xfd, ..] => return scan_into(&mut BufReader::new(zstd::Decoder::new(reader)?), scan),
            [0x1f, 0x8b, ..] => return scan_into(&mut BufReader::new(flate2::bufread::MultiGzDecoder::new(reader)), scan),
            // xz, or the older bare LZMA format
            [0xfd, b'7', b'z', b'X', b'Z', ..] | [0x5d, 0, 0, ..] => {
                let stream = liblzma::stream::Stream::new_auto_decoder(u64::MAX, 0).map_err(io::Error::other)?;
                return scan_into(&mut BufReader::new(liblzma::bufread::XzDecoder::new_stream(reader, stream)), scan);
            }
            [0x04, 0x22, 0x4d, 0x18, ..] => return scan_into(&mut BufReader::new(lz4_flex::frame::FrameDecoder::new(reader)), scan),
            // lz4's legacy format, which is what the kernel accepts: blocks of at most 8 MiB.
            [0x02, 0x21, 0x4c, 0x18, ..] => {
                reader.consume(4);
                let mut plain = Vec::new();
                let mut size = [0; 4];
                while reader.read_exact(&mut size).is_ok() {
                    let mut block = vec![0; u32::from_le_bytes(size) as usize];
                    if block.len() > 16 << 20 || reader.read_exact(&mut block).is_err() {
                        break; // trailing bytes that are not a block
                    }
                    plain.extend(lz4_flex::block::decompress(&block, 8 << 20).map_err(io::Error::other)?);
                }
                return scan_into(&mut plain.as_slice(), scan);
            }
            other => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown initrd format {other:02x?}"))),
        }
    }
}

/// A file or directory to add to an initrd.
pub struct Added {
    pub path: String,
    pub mode: u32,
    pub content: Vec<u8>,
}

impl Added {
    pub fn file(path: &str, mode: u32, content: Vec<u8>) -> Added {
        Added { path: path.into(), mode: 0o100000 | mode, content }
    }
}

/// A cpio archive of `entries`. Missing parent directories are created
/// first: the kernel does not create them, and silently drops the file.
pub fn archive(entries: &[Added]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut made = std::collections::HashSet::new();
    let mut ino = 0;
    let mut write = |out: Vec<u8>, path: &str, mode: u32, content: &[u8]| -> io::Result<Vec<u8>> {
        ino += 1;
        let mut writer = newc::Builder::new(path).mode(mode).ino(ino).nlink(1).write(out, content.len() as u32);
        writer.write_all(content)?;
        writer.finish()
    };
    for entry in entries {
        let parents: Vec<&str> = entry.path.match_indices('/').map(|(i, _)| &entry.path[..i]).collect();
        for dir in parents {
            if made.insert(dir.to_string()) {
                out = write(out, dir, 0o040755, &[])?;
            }
        }
        out = write(out, &entry.path, entry.mode, &entry.content)?;
    }
    newc::trailer(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        // A microcode archive, padding, then the main archive: the usual layout.
        let mut image = archive(&[Added::file("kernel/x86/microcode/GenuineIntel.bin", 0o644, vec![1, 2, 3])]).unwrap();
        image.extend([0; 512]);
        image
    }

    fn main_archive() -> Vec<u8> {
        archive(&[
            Added { path: "lib".into(), mode: SYMLINK | 0o777, content: b"usr/lib".to_vec() },
            Added::file("usr/lib/modules/6.12.0-1-amd64/kernel/drivers/block/nbd.ko.xz", 0o644, vec![0]),
            Added { path: "init".into(), mode: SYMLINK | 0o777, content: b"usr/lib/systemd/systemd".to_vec() },
            // The start of an ARM64 program: its machine type is the 16-bit number at byte 18.
            Added::file("usr/bin/busybox", 0o755, [b"\x7fELF".as_slice(), &[0; 14], &[183, 0, 9, 9]].concat()),
        ])
        .unwrap()
    }

    fn expected() -> Scan {
        Scan { init: Some((SYMLINK | 0o777, b"usr/lib/systemd/systemd".to_vec())), has_nbd: true, lib_is_link: true, kernel: Some("6.12.0-1-amd64".into()), machine: Some(183) }
    }

    #[test]
    fn scans_plain_archives() {
        let image = [sample(), main_archive()].concat();
        assert_eq!(scan(image.as_slice()).unwrap(), expected());
    }

    #[test]
    fn scans_every_compression() {
        let plain = main_archive();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&plain).unwrap();
        let mut xz = liblzma::write::XzEncoder::new(Vec::new(), 1);
        xz.write_all(&plain).unwrap();
        let mut lz4 = lz4_flex::frame::FrameEncoder::new(Vec::new());
        lz4.write_all(&plain).unwrap();
        let block = lz4_flex::block::compress(&plain);
        let legacy = [&[0x02, 0x21, 0x4c, 0x18][..], &(block.len() as u32).to_le_bytes(), &block].concat();
        let packed = [
            ("zstd", zstd::encode_all(plain.as_slice(), 1).unwrap()),
            ("gzip", gzip.finish().unwrap()),
            ("xz", xz.finish().unwrap()),
            ("lz4", lz4.finish().unwrap()),
            ("lz4 legacy", legacy),
        ];
        for (name, compressed) in packed {
            let image = [sample(), compressed].concat();
            assert_eq!(scan(image.as_slice()).unwrap(), expected(), "{name}");
        }
    }

    #[test]
    fn a_shell_script_init_and_no_modules() {
        let image = archive(&[Added::file("init", 0o755, b"#!/bin/sh\n".to_vec())]).unwrap();
        let found = scan(image.as_slice()).unwrap();
        assert_eq!(found, Scan { init: Some((0o100755, b"#!/bin/sh\n".to_vec())), ..Scan::default() });
    }

    #[test]
    fn junk_is_an_error() {
        assert!(scan(&b"this is not an initrd"[..]).is_err());
    }
}
