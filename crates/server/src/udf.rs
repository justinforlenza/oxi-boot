//! The files of a UDF disc, which is what a Windows ISO is. The `libmkiso`
//! crate reads the file system; its reader cannot move between threads, so
//! the names and the places of the files are copied out of it once.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use libmkiso::topology::Parent;
use libmkiso::udf::{Limits, UdfReader};

pub struct Node {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Indexes into the tree, for a folder.
    pub children: Vec<usize>,
    /// (offset in the file, offset in the ISO or None for a hole, length)
    extents: Vec<(u64, Option<u64>, u64)>,
}

pub struct Tree {
    file: File,
    /// Node 0 is the top folder.
    nodes: Vec<Node>,
}

struct Source(File, u64);

impl libmkiso::ReadAt for Source {
    fn len(&self) -> u64 {
        self.1
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_at(buf, offset)
    }
}

impl Tree {
    /// Fails for an ISO that has no UDF file system.
    pub fn open(file: File) -> io::Result<Tree> {
        let source = Source(file.try_clone()?, file.metadata()?.len());
        // Sizes are not limited: an install image alone can pass 8 GiB.
        let limits = Limits { max_entry_bytes: u64::MAX, max_total_bytes: u64::MAX, max_input_bytes: u64::MAX, ..Limits::default() };
        let reader = UdfReader::open_source(source, limits).map_err(|e| io::Error::other(e.to_string()))?;
        let folder = |name: &str| Node { name: name.into(), dir: true, size: 0, children: Vec::new(), extents: Vec::new() };
        let mut nodes = vec![folder("")];
        // Entry i becomes node i + 1. An entry with no parent (a named stream) stays out of reach.
        // The reader names an entry by its whole path; the tree wants the last part.
        for (i, entry) in reader.entries().iter().enumerate() {
            let mut node = Node { dir: entry.directory, size: entry.size, ..folder(entry.name.rsplit('/').next().unwrap_or_default()) };
            if !entry.directory {
                reader
                    .visit_extents(i, |at, place, len| {
                        node.extents.push((at, place, len));
                        Ok(())
                    })
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
            nodes.push(node);
        }
        for i in 0..reader.entries().len() {
            match reader.parent(i) {
                Some(Parent::Root) => nodes[0].children.push(i + 1),
                Some(Parent::Entry(parent)) => nodes[parent + 1].children.push(i + 1),
                None => {}
            }
        }
        Ok(Tree { file, nodes })
    }

    pub fn node(&self, index: usize) -> &Node {
        &self.nodes[index]
    }

    /// The node at `path`, whose parts are separated by `/` or `\`. Case is
    /// ignored, as Windows expects.
    pub fn find(&self, path: &str) -> Option<usize> {
        path.split(['/', '\\']).filter(|part| !part.is_empty()).try_fold(0, |at, part| {
            self.nodes[at].children.iter().copied().find(|&child| self.nodes[child].name.eq_ignore_ascii_case(part))
        })
    }

    /// Up to `len` bytes of a file from `offset`; fewer only at its end.
    pub fn read(&self, index: usize, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let node = &self.nodes[index];
        let mut out = vec![0; node.size.saturating_sub(offset).min(len as u64) as usize];
        let end = offset + out.len() as u64;
        for &(at, place, extent_len) in &node.extents {
            let (from, to) = (at.max(offset), (at + extent_len).min(end));
            if let (true, Some(place)) = (from < to, place) {
                self.file.read_exact_at(&mut out[(from - offset) as usize..(to - offset) as usize], place + from - at)?;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_across_extents_and_holes() {
        let path = std::env::temp_dir().join(format!("oxi-udf-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let file = File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let node = |name: &str, dir, children, extents| Node { name: name.into(), dir, size: 9, children, extents };
        // The file is "789", then a hole of three bytes, then "012".
        let tree = Tree {
            file,
            nodes: vec![
                node("", true, vec![1], vec![]),
                node("Sources", true, vec![2], vec![]),
                node("boot.wim", false, vec![], vec![(0, Some(7), 3), (3, None, 3), (6, Some(0), 3)]),
            ],
        };
        assert_eq!(tree.find("sources\\BOOT.WIM"), Some(2));
        assert_eq!(tree.find("/sources/missing"), None);
        assert_eq!(tree.read(2, 0, 100).unwrap(), b"789\0\0\x00012");
        assert_eq!(tree.read(2, 2, 5).unwrap(), b"9\0\0\x000");
        assert_eq!(tree.read(2, 9, 5).unwrap(), b"");
    }
}
