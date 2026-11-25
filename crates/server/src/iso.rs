//! ISO 9660, far enough to list an image's files and to serve a patched view
//! of it in which some files have new content.
//!
//! This reader is ours because the patch rewrites directory records in place
//! and so needs each record's position in the image, which no published ISO
//! crate exposes.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

const SECTOR: u64 = 2048;
/// iPXE on BIOS cannot read a network disk past this offset.
const BIOS_LIMIT: u64 = 1 << 32;

#[derive(Debug, Clone)]
pub struct Record {
    /// Absolute offset of the directory record in the image.
    pos: u64,
    pub lba: u32,
    pub len: u32,
    /// Path from the root, without a leading slash.
    pub path: String,
}

impl Record {
    pub fn offset(&self) -> u64 {
        self.lba as u64 * SECTOR
    }

    /// The last path component, in lower case.
    pub fn name(&self) -> String {
        self.path.rsplit('/').next().unwrap_or("").to_ascii_lowercase()
    }
}

fn read_at(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; len];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

/// Root directories as (lba, len, is_joliet): the primary tree and any Joliet trees.
fn roots(file: &File) -> io::Result<Vec<(u32, u32, bool)>> {
    let mut roots = Vec::new();
    for sector in 16..64 {
        let d = read_at(file, sector * SECTOR, SECTOR as usize)?;
        if &d[1..6] != b"CD001" || d[0] == 255 {
            break;
        }
        if d[0] == 1 || d[0] == 2 {
            roots.push((le32(&d[158..]), le32(&d[166..]), d[0] == 2));
        }
    }
    Ok(roots)
}

/// One directory's entries as (record, is_dir), with `path` holding the bare name.
fn read_dir(file: &File, lba: u32, len: u32, joliet: bool) -> io::Result<Vec<(Record, bool)>> {
    let base = lba as u64 * SECTOR;
    let data = read_at(file, base, len as usize)?;
    let mut records = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let l = data[i] as usize;
        if l == 0 {
            i = (i / SECTOR as usize + 1) * SECTOR as usize; // records never span sectors
            continue;
        }
        let Some(rec) = data.get(i..i + l).filter(|rec| rec.len() >= 34 && 33 + rec[32] as usize <= rec.len()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged ISO directory"));
        };
        let name_len = rec[32] as usize;
        let raw = &rec[33..33 + name_len];
        if raw != [0] && raw != [1] {
            let mut name = if joliet {
                let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                String::from_utf16_lossy(&units)
            } else {
                // Rock Ridge NM entries hold the real name. shortcut: NM in a
                // continuation area (CE) is not followed; such a file is known
                // by its 8.3 name. Follow CE if an initrd is ever missed for it.
                let mut su = rec.get((33 + name_len + 1) & !1..).unwrap_or(&[]);
                let mut rr = Vec::new();
                while su.len() >= 5 && su[2] as usize >= 5 && su[2] as usize <= su.len() {
                    if &su[..2] == b"NM" {
                        rr.extend_from_slice(&su[5..su[2] as usize]);
                    }
                    su = &su[su[2] as usize..];
                }
                String::from_utf8_lossy(if rr.is_empty() { raw } else { &rr }).into_owned()
            };
            if let Some(n) = name.find(';') {
                name.truncate(n);
            }
            let path = name.trim_end_matches('.').to_string();
            records.push((Record { pos: base + i as u64, lba: le32(&rec[2..]), len: le32(&rec[10..]), path }, rec[25] & 2 != 0));
        }
        i += l;
    }
    Ok(records)
}

/// Every file's directory records, from every tree, so a file on a disc with
/// a Joliet tree appears twice. Empty for an image that is not ISO 9660.
pub fn files(file: &File) -> io::Result<Vec<Record>> {
    let mut found = Vec::new();
    for (lba, len, joliet) in roots(file)? {
        let (mut todo, mut seen) = (vec![(lba, len, String::new())], HashSet::new());
        while let Some((lba, len, prefix)) = todo.pop() {
            if !seen.insert(lba) {
                continue;
            }
            for (mut record, is_dir) in read_dir(file, lba, len, joliet)? {
                record.path = format!("{prefix}{}", record.path);
                match is_dir {
                    true => todo.push((record.lba, record.len, format!("{}/", record.path))),
                    false => found.push(record),
                }
            }
        }
    }
    Ok(found)
}

/// Part of a file's new content.
pub enum Piece {
    /// Bytes that are already in the ISO.
    Iso { offset: u64, len: u64 },
    Bytes(Vec<u8>),
}

impl Piece {
    fn len(&self) -> u64 {
        match self {
            Piece::Iso { len, .. } => *len,
            Piece::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

/// The image as clients see it: the ISO with overlays on top.
pub struct View {
    iso_len: u64,
    /// (absolute offset, content), applied in order.
    overlays: Vec<(u64, Piece)>,
    size: u64,
}

impl View {
    pub fn unpatched(file: &File) -> io::Result<View> {
        let iso_len = file.metadata()?.len();
        Ok(View { iso_len, overlays: Vec::new(), size: iso_len })
    }

    /// Gives the files at the given lbas new content, each in one piece of
    /// the image, and points their directory records at it.
    ///
    /// The new content is laid over large files that are not being moved, on
    /// the bet that a boot loader never reads those (on every ISO tried the
    /// largest files are the root file system image or packages). The helper
    /// reads the untouched ISO, so nothing after the boot loader sees an
    /// overlay. Only files below 4 GiB are used, because iPXE on BIOS cannot
    /// read past that. Content that fits nowhere goes after the ISO's end.
    /// `may_host` names the files that may be laid over.
    pub fn replace(file: &File, records: &[Record], mut moved: Vec<(u32, Vec<Piece>)>, may_host: impl Fn(&Record) -> bool) -> io::Result<View> {
        let iso_len = file.metadata()?.len();
        let size_of = |pieces: &[Piece]| pieces.iter().map(Piece::len).sum::<u64>();
        moved.sort_by_key(|(_, pieces)| std::cmp::Reverse(size_of(pieces)));

        // Spare regions as (next free offset, end), largest first.
        let mut hosts: Vec<(u64, u64, &str)> = records
            .iter()
            .filter(|r| moved.iter().all(|(lba, _)| *lba != r.lba) && r.offset() < BIOS_LIMIT && may_host(r))
            .map(|r| (r.offset(), (r.offset() + r.len as u64).min(BIOS_LIMIT), r.path.as_str()))
            .collect();
        hosts.sort_by_key(|(start, end, _)| std::cmp::Reverse(end - start));
        let mut seen = HashSet::new();
        hosts.retain(|(start, ..)| seen.insert(*start)); // the same file in two trees
        let mut end = iso_len.next_multiple_of(SECTOR);

        let mut overlays = Vec::new();
        for (lba, pieces) in moved {
            let (len, need) = (size_of(&pieces), size_of(&pieces).next_multiple_of(SECTOR));
            let start = match hosts.iter_mut().find(|(start, end, _)| end - start >= need) {
                Some((start, _, path)) => {
                    eprintln!("patch: {len} bytes laid over {path}");
                    *start += need;
                    *start - need
                }
                None => {
                    eprintln!("patch: {len} bytes placed after the ISO's end{}", if end + need > BIOS_LIMIT { " (out of reach on BIOS)" } else { "" });
                    end += need;
                    end - need
                }
            };
            // Both fields are stored little-endian then big-endian.
            let (new_lba, new_len) = ((start / SECTOR) as u32, len as u32);
            for r in records.iter().filter(|r| r.lba == lba) {
                overlays.push((r.pos + 2, Piece::Bytes([new_lba.to_le_bytes(), new_lba.to_be_bytes()].concat())));
                overlays.push((r.pos + 10, Piece::Bytes([new_len.to_le_bytes(), new_len.to_be_bytes()].concat())));
            }
            let mut at = start;
            for piece in pieces {
                let len = piece.len();
                overlays.push((at, piece));
                at += len;
            }
            // Zeros up to the sector boundary, not the tail of the file underneath.
            overlays.push((at, Piece::Bytes(vec![0; (start + need - at) as usize])));
        }
        // Zero padding after content placed past the end: GRUB reads in 32 KiB
        // blocks, and a block that crosses the end of the disk is refused.
        let size = if end > iso_len { (end + (1 << 20)).next_multiple_of(1 << 20) } else { iso_len };
        Ok(View { iso_len, overlays, size })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn is_patched(&self) -> bool {
        !self.overlays.is_empty()
    }

    pub fn read(&self, file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; len];
        let end = offset + len as u64;
        if offset < self.iso_len {
            let n = (self.iso_len.min(end) - offset) as usize;
            file.read_exact_at(&mut buf[..n], offset)?;
        }
        for (start, piece) in &self.overlays {
            let (from, to) = ((*start).max(offset), (start + piece.len()).min(end));
            if from >= to {
                continue;
            }
            let out = &mut buf[(from - offset) as usize..(to - offset) as usize];
            match piece {
                Piece::Bytes(bytes) => out.copy_from_slice(&bytes[(from - start) as usize..(to - start) as usize]),
                Piece::Iso { offset: source, .. } => file.read_exact_at(out, source + (from - start))?,
            }
        }
        Ok(buf)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    /// A directory record for a file, in both byte orders, as on a real disc.
    fn record(name: &str, lba: u32, len: u32, dir: bool) -> Vec<u8> {
        let mut r = vec![0; 33];
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[6..10].copy_from_slice(&lba.to_be_bytes());
        r[10..14].copy_from_slice(&len.to_le_bytes());
        r[14..18].copy_from_slice(&len.to_be_bytes());
        r[25] = if dir { 2 } else { 0 };
        r[32] = name.len() as u8;
        r.extend(name.bytes());
        r.resize((r.len() + 1) & !1, 0);
        r[0] = r.len() as u8;
        r
    }

    /// A tiny image: root (sector 20) holds BOOT/ (sector 21), which holds
    /// INITRD.IMG (sector 30, "old-initrd"); the root also holds ROOTFS.SFS
    /// (sector 40, 3 sectors). Returns the open file.
    pub fn image(name: &str) -> File {
        image_with_initrd(name, b"old-initrd")
    }

    /// The same image with other content (under 20 KiB) as its INITRD.IMG.
    pub fn image_with_initrd(name: &str, initrd: &[u8]) -> File {
        let mut data = vec![0u8; 50 * SECTOR as usize];
        let at = |sector: usize| sector * SECTOR as usize;
        data[at(16)] = 1;
        data[at(16) + 1..at(16) + 6].copy_from_slice(b"CD001");
        data[at(16) + 158..at(16) + 162].copy_from_slice(&20u32.to_le_bytes());
        data[at(16) + 166..at(16) + 170].copy_from_slice(&2048u32.to_le_bytes());
        data[at(17)] = 255;
        data[at(17) + 1..at(17) + 6].copy_from_slice(b"CD001");
        let root = [record("\0", 20, 2048, true), record("BOOT", 21, 2048, true), record("ROOTFS.SFS;1", 40, 3 * 2048, false)].concat();
        data[at(20)..at(20) + root.len()].copy_from_slice(&root);
        let boot = [record("\0", 21, 2048, true), record("\x01", 20, 2048, true), record("INITRD.IMG;1", 30, initrd.len() as u32, false)].concat();
        data[at(21)..at(21) + boot.len()].copy_from_slice(&boot);
        data[at(30)..at(30) + initrd.len()].copy_from_slice(initrd);
        data[at(40)..at(43)].fill(7);
        let path = std::env::temp_dir().join(format!("oxi-iso-test-{name}-{}", std::process::id()));
        File::create(&path).unwrap().write_all(&data).unwrap();
        let file = File::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        file
    }

    #[test]
    fn lists_files_and_moves_one() {
        let file = image("view");
        let records = files(&file).unwrap();
        let mut paths: Vec<_> = records.iter().map(|r| (r.path.as_str(), r.lba, r.len)).collect();
        paths.sort();
        assert_eq!(paths, [("BOOT/INITRD.IMG", 30, 10), ("ROOTFS.SFS", 40, 6144)]);

        let pieces = vec![Piece::Iso { offset: 30 * SECTOR, len: 10 }, Piece::Bytes(b"+new".to_vec())];
        let view = View::replace(&file, &records, vec![(30, pieces)], |_| true).unwrap();
        assert_eq!(view.size(), 50 * SECTOR); // laid over ROOTFS.SFS, so no longer
        // The directory record now points at sector 40 with the new length...
        let moved = files_in_view(&view, &file);
        assert!(moved.iter().any(|r| (r.path.as_str(), r.lba, r.len) == ("BOOT/INITRD.IMG", 40, 14)));
        // ...where the new content sits, padded with zeros to the sector's end,
        // and the rest of the host file is untouched.
        assert_eq!(view.read(&file, 40 * SECTOR, 16).unwrap(), b"old-initrd+new\0\0");
        assert_eq!(view.read(&file, 41 * SECTOR - 1, 2).unwrap(), [0, 7]);
        assert_eq!(view.read(&file, 30 * SECTOR, 10).unwrap(), b"old-initrd");
    }

    #[test]
    fn content_that_fits_nowhere_goes_after_the_end() {
        let file = image("tail");
        let records = files(&file).unwrap();
        let big = vec![Piece::Bytes(vec![9; 4 * SECTOR as usize])];
        let view = View::replace(&file, &records, vec![(30, big)], |_| true).unwrap();
        assert_eq!(view.size(), 2 << 20);
        assert_eq!(view.read(&file, 50 * SECTOR - 1, 2).unwrap(), [0, 9]);
        assert_eq!(view.read(&file, 54 * SECTOR - 1, 2).unwrap(), [9, 0]);
    }

    /// Lists the files as a client of the view would see them.
    pub fn files_in_view(view: &View, file: &File) -> Vec<Record> {
        let bytes = view.read(file, 0, view.size() as usize).unwrap();
        let path = std::env::temp_dir().join(format!("oxi-iso-test-through-{}-{:p}", std::process::id(), view));
        std::fs::write(&path, bytes).unwrap();
        let records = files(&File::open(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        records
    }
}
