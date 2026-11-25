//! Windows ISOs. They cannot boot from a virtual CD over the network: once
//! the installer environment is up it no longer sees the CD. So iPXE loads
//! that environment with wimboot, and a startup script added to it connects
//! to a file share on this server and starts Setup from there. The share is
//! served by the `smb-server` crate and reads its files straight out of the ISOs.

use std::collections::HashMap;
use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use smb_server::{
    Access, BackendCapabilities, Bytes, DirEntry, FileInfo, FileTimes, Handle, OpenAction, OpenIntent, OpenOptions, Share, ShareBackend, SmbError,
    SmbPath, SmbResult, SmbServer,
};

use crate::udf::Tree;

const SHARE: &str = "oxi";
const USER: &str = "oxi";

/// Tells the installer environment to run our script in place of Setup.
pub const WINPESHL: &str = "[LaunchApps]\r\n\"install.bat\"\r\n";

/// The ISO folder as Windows discs: each ISO's file tree, read once and kept
/// until the ISO changes.
pub struct Isos {
    dir: PathBuf,
    ip: Ipv4Addr,
    password: String,
    trees: Mutex<HashMap<String, Cached>>,
}

/// An ISO's modification time and size when it was read, and its tree if it has one.
type Cached = ((SystemTime, u64), Option<Arc<Tree>>);

impl Isos {
    pub fn new(dir: PathBuf, ip: Ipv4Addr) -> std::io::Result<Isos> {
        // Windows refuses to log in to a share as a guest, so there must be a
        // password. shortcut: it is not a secret, any client can read it in
        // its startup script; the share is read-only and holds only the ISOs
        // that HTTP already serves. Upgrade if the share ever holds more.
        let mut random = [0; 8];
        std::io::Read::read_exact(&mut File::open("/dev/urandom")?, &mut random)?;
        let password = random.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Isos { dir, ip, password, trees: Mutex::default() })
    }

    /// The files of the ISO at `path` under the ISO folder, if it is a UDF disc.
    pub fn tree(&self, path: &str) -> Option<Arc<Tree>> {
        let inside = path.split('/').all(|part| !part.is_empty() && !part.starts_with('.'));
        let file = File::open(self.dir.join(path)).ok().filter(|_| inside)?;
        let meta = file.metadata().ok().filter(|meta| meta.is_file())?;
        let key = (meta.modified().ok()?, meta.len());
        let mut trees = self.trees.lock().unwrap();
        match trees.get(path) {
            Some((seen, tree)) if *seen == key => tree.clone(),
            _ => {
                let tree = Tree::open(file).ok().map(Arc::new);
                trees.insert(path.to_string(), (key, tree.clone()));
                tree
            }
        }
    }

    /// The startup script for the ISO at `path`.
    // shortcut: a `%` in an ISO's path is not escaped for the batch file.
    pub fn install_bat(&self, path: &str) -> String {
        let share = format!("\\\\{}\\{SHARE}\\{}", self.ip, path.replace('/', "\\"));
        // Windows 11's Setup hands over to another program and returns at once,
        // and the machine restarts when this script ends. So it ends in a shell.
        format!(
            "@echo off\r\n\
             echo oxi-boot: starting the network\r\n\
             wpeinit\r\n\
             :connect\r\n\
             echo oxi-boot: connecting to {share}\r\n\
             net use Z: \"{share}\" {} /user:{USER} && goto run\r\n\
             echo oxi-boot: no connection yet, trying again\r\n\
             ping -n 4 127.0.0.1 >nul\r\n\
             goto connect\r\n\
             :run\r\n\
             Z:\\setup.exe\r\n\
             echo oxi-boot: if Setup has closed, type exit to restart.\r\n\
             cmd /k\r\n",
            self.password
        )
    }

    /// Serves the share on port 445 until it fails.
    pub async fn serve(self: Arc<Self>) -> Result<(), String> {
        let (ip, password) = (self.ip, self.password.clone());
        let server = SmbServer::builder()
            .listen(SocketAddr::from((ip, 445)))
            .user(USER, password)
            .share(Share::new(SHARE, Backend(self)).user(USER, Access::Read))
            .build()
            .map_err(|e| e.to_string())?;
        server.bind().await.map_err(|e| format!("port 445: {e}"))?;
        server.serve().await.map_err(|e| e.to_string())
    }
}

/// How a Windows disc boots: `None` if `tree` is not one, otherwise whether
/// it is an installer (it has Setup and an install image) or some other
/// Windows PE disc, which keeps its own startup.
pub fn installer(tree: &Tree) -> Option<bool> {
    let has = |path: &str| tree.find(path).is_some();
    let boots = has("boot/bcd") && has("boot/boot.sdi") && has("sources/boot.wim");
    boots.then(|| has("setup.exe") && ["wim", "esd", "swm"].iter().any(|ext| has(&format!("sources/install.{ext}"))))
}

/// The iPXE script that boots a Windows disc. `files` is the URL its files are under.
pub fn script(base: &str, url: &str, installer: bool) -> String {
    let file = |name: &str, path: &str| format!("initrd -n {name} {base}/win/{url}?file={path} {name}\n");
    let ours = if installer { file("winpeshl.ini", "winpeshl.ini") + &file("install.bat", "install.bat") } else { String::new() };
    format!("#!ipxe\nkernel {base}/wimboot\n{ours}{}{}{}boot\n", file("BCD", "boot/bcd"), file("boot.sdi", "boot/boot.sdi"), file("boot.wim", "sources/boot.wim"))
}

/// The share: `\<ISO path>\<path inside that ISO>`. The folders of the ISO
/// folder itself can be opened but show as empty.
struct Backend(Arc<Isos>);

/// An open file or folder of an ISO, or (without a tree) a folder outside any ISO.
struct Open {
    tree: Option<Arc<Tree>>,
    node: usize,
    name: String,
}

#[async_trait::async_trait]
impl ShareBackend for Backend {
    async fn open(&self, path: &SmbPath, opts: OpenOptions) -> SmbResult<(Box<dyn Handle>, OpenAction)> {
        if opts.write || opts.delete_on_close || !matches!(opts.intent, OpenIntent::Open) {
            return Err(SmbError::AccessDenied);
        }
        let parts = path.components();
        let name = parts.last().cloned().unwrap_or_default();
        // The ISO is the shortest start of the path that names a file.
        let iso = (1..=parts.len()).find(|&n| self.0.dir.join(parts[..n].join("/")).is_file());
        let open = match iso {
            Some(n) => {
                let tree = self.0.tree(&parts[..n].join("/")).ok_or(SmbError::NotFound)?;
                let node = tree.find(&parts[n..].join("/")).ok_or(SmbError::NotFound)?;
                Open { tree: Some(tree), node, name }
            }
            None if parts.iter().all(|part| !part.starts_with('.')) && self.0.dir.join(parts.join("/")).is_dir() => Open { tree: None, node: 0, name },
            None => return Err(SmbError::NotFound),
        };
        let dir = open.tree.as_ref().is_none_or(|tree| tree.node(open.node).dir);
        match (dir, opts.directory, opts.non_directory) {
            (false, true, _) => Err(SmbError::NotADirectory),
            (true, _, true) => Err(SmbError::IsDirectory),
            _ => Ok((Box::new(open), OpenAction::Opened)),
        }
    }
    async fn unlink(&self, _: &SmbPath) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    async fn rename(&self, _: &SmbPath, _: &SmbPath, _: bool) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    fn capabilities(&self) -> BackendCapabilities {
        let mut capabilities = BackendCapabilities::default();
        capabilities.is_read_only = true;
        capabilities
    }
}

impl Open {
    fn info(&self, name: &str, node: usize) -> FileInfo {
        let mut info = FileInfo::new(name);
        info.read_only = true;
        info.is_directory = true;
        if let Some(tree) = &self.tree {
            let node_ref = tree.node(node);
            info.is_directory = node_ref.dir;
            info.end_of_file = node_ref.size;
            info.allocation_size = node_ref.size.next_multiple_of(2048);
            info.file_index = node as u64 + 1;
        }
        info
    }
}

#[async_trait::async_trait]
impl Handle for Open {
    async fn read(&self, offset: u64, len: u32) -> SmbResult<Bytes> {
        let tree = self.tree.as_ref().ok_or(SmbError::IsDirectory)?;
        // A read from the page cache or the disk; done here, off the other tasks' way.
        Ok(tokio::task::block_in_place(|| tree.read(self.node, offset, len as usize))?.into())
    }
    async fn write(&self, _: u64, _: &[u8]) -> SmbResult<u32> {
        Err(SmbError::AccessDenied)
    }
    async fn flush(&self) -> SmbResult<()> {
        Ok(())
    }
    async fn stat(&self) -> SmbResult<FileInfo> {
        Ok(self.info(&self.name, self.node))
    }
    async fn set_times(&self, _: FileTimes) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    async fn set_attributes(&self, _: u32) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    async fn truncate(&self, _: u64) -> SmbResult<()> {
        Err(SmbError::AccessDenied)
    }
    async fn list_dir(&self, pattern: Option<&str>) -> SmbResult<Vec<DirEntry>> {
        let Some(tree) = &self.tree else { return Ok(Vec::new()) };
        let children = tree.node(self.node).children.iter().map(|&child| (child, &tree.node(child).name));
        Ok(children.filter(|(_, name)| pattern.is_none_or(|p| matches(p, name))).map(|(child, name)| DirEntry { info: self.info(name, child) }).collect())
    }
    async fn close(self: Box<Self>) -> SmbResult<()> {
        Ok(())
    }
}

/// Whether `name` fits a Windows search pattern: `*` and `?`, their old DOS
/// forms `<` and `>`, and `"` for a dot. Case is ignored.
fn matches(pattern: &str, name: &str) -> bool {
    fn go(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*' | '<', rest)) => (0..=name.len()).any(|skip| go(rest, &name[skip..])),
            Some(('?' | '>', rest)) => !name.is_empty() && go(rest, &name[1..]),
            Some((&c, rest)) => {
                let c = if c == '"' { '.' } else { c };
                name.first().is_some_and(|n| n.eq_ignore_ascii_case(&c)) && go(rest, &name[1..])
            }
        }
    }
    go(&pattern.chars().collect::<Vec<_>>(), &name.chars().collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_patterns() {
        assert!(matches("*", "setup.exe"));
        assert!(matches("SETUP.EXE", "setup.exe"));
        assert!(matches("*.wim", "install.wim"));
        assert!(matches("install<\"wim", "install.wim"));
        assert!(matches("boot.w?m", "boot.wim"));
        assert!(!matches("*.esd", "install.wim"));
        assert!(!matches("setup", "setup.exe"));
    }

    #[test]
    fn installer_script_adds_our_startup_and_a_pe_disc_keeps_its_own() {
        let with = script("http://10.0.0.1", "win/Win%2011.iso", true);
        assert!(with.contains("initrd -n install.bat http://10.0.0.1/win/win/Win%2011.iso?file=install.bat install.bat\n"));
        assert!(with.contains("initrd -n boot.wim http://10.0.0.1/win/win/Win%2011.iso?file=sources/boot.wim boot.wim\n"));
        assert!(!script("http://10.0.0.1", "pe.iso", false).contains("install.bat"));
    }
}
