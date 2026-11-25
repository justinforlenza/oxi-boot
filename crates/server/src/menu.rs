//! The boot menu: the ISO folder as an iPXE script.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::config::{Config, IsoOverride};

/// Everything but letters, digits and these is escaped in an ISO's URL.
const URL_PATH: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~').remove(b'/');

pub fn url_path(path: &str) -> String {
    utf8_percent_encode(path, URL_PATH).to_string()
}

#[derive(Debug, Default, PartialEq)]
pub struct Folder {
    name: String,
    folders: Vec<Folder>,
    /// (name shown, path under the ISO folder)
    isos: Vec<(String, String)>,
}

impl Folder {
    /// Every ISO's path, folders included.
    pub fn paths(&self) -> Vec<&str> {
        let below = self.folders.iter().flat_map(Folder::paths);
        self.isos.iter().map(|(_, path)| path.as_str()).chain(below).collect()
    }
}

/// The ISOs under `dir`, sorted by name. Hidden ISOs, dot files and folders
/// with no ISO in them are left out.
pub fn scan(dir: &Path, prefix: &str, overrides: &HashMap<String, IsoOverride>) -> Folder {
    let mut folder = Folder { name: prefix.trim_end_matches('/').rsplit('/').next().unwrap_or("").into(), ..Folder::default() };
    let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{prefix}{name}");
        if name.starts_with('.') {
            continue;
        }
        // metadata, not file_type: a link to an ISO or a folder counts.
        let Ok(meta) = std::fs::metadata(entry.path()) else { continue };
        if meta.is_dir() {
            let sub = scan(&entry.path(), &format!("{path}/"), overrides);
            if !sub.folders.is_empty() || !sub.isos.is_empty() {
                folder.folders.push(sub);
            }
        } else if name.to_ascii_lowercase().ends_with(".iso") {
            let over = overrides.get(&path);
            if !over.is_some_and(|o| o.hidden) {
                let shown = over.and_then(|o| o.name.clone()).unwrap_or_else(|| name[..name.len() - 4].to_string());
                folder.isos.push((shown, path));
            }
        }
    }
    folder
}

const LOCAL_DISK: &str = "iseq ${platform} efi && exit ||\nsanboot --no-describe --drive 0x80 || exit\n";

/// What a machine that is not allowed to network boot runs.
pub fn denied() -> String {
    format!("#!ipxe\necho oxi-boot: this machine is not allowed to network boot\n{LOCAL_DISK}")
}

/// The whole menu. `base` is the server's URL, without a trailing slash.
pub fn script(root: &Folder, config: &Config, base: &str) -> String {
    // Folders become labels f0, f1, ... (f0 is the top menu) and ISOs i0, i1, ...
    let (mut menus, mut boots, mut default) = (String::new(), String::new(), "local".to_string());
    let mut todo = vec![(root, "f0".to_string(), None::<String>)];
    let (mut folders, mut isos, mut default_item) = (1, 0, String::new());
    while let Some((folder, label, parent)) = todo.pop() {
        let title = if parent.is_some() { folder.name.as_str() } else { "oxi-boot" };
        write!(menus, ":{label}\nmenu {title}\n").unwrap();
        if let Some(parent) = &parent {
            writeln!(menus, "item {parent} ..").unwrap();
        }
        for sub in &folder.folders {
            let sub_label = format!("f{folders}");
            folders += 1;
            writeln!(menus, "item {sub_label} {}/", sub.name).unwrap();
            todo.push((sub, sub_label, Some(label.clone())));
        }
        for (shown, path) in &folder.isos {
            writeln!(menus, "item i{isos} {shown}").unwrap();
            // Patched unless the config says otherwise. How a patched ISO boots is
            // only known once its patch is worked out, so that is a script of its own.
            let boot = match config.iso.get(path).is_some_and(|o| o.as_is) {
                true => format!("sanboot {base}/as-is/{}", url_path(path)),
                false => format!("chain {base}/boot/{}", url_path(path)),
            };
            writeln!(boots, ":i{isos}\n{boot} || goto failed").unwrap();
            if *path == config.menu.default {
                default = format!("i{isos}");
                if parent.is_some() {
                    default_item = format!("item i{isos} {shown}\n");
                }
            }
            isos += 1;
        }
        match parent {
            // Esc in a folder goes back to the top menu.
            Some(_) => menus.push_str("choose target && goto ${target} || goto f0\n"),
            None => {
                // The timeout can only pick an entry of this menu, so a default
                // from a folder is also listed here; filled in below.
                menus.push_str("DEFAULT_ITEM");
                menus.push_str("item --gap\n");
                if !root.paths().is_empty() {
                    menus.push_str("item asis Boot an ISO as-is (unpatched)...\n");
                }
                menus.push_str("item local Boot from local disk\nitem reboot Reboot\nitem firmware Firmware setup\nitem shell iPXE shell\n");
                let timeout = match config.menu.timeout {
                    0 => String::new(),
                    seconds => format!("--timeout {} ", seconds * 1000),
                };
                // The default may sit in a folder not visited yet; its label is filled in below.
                writeln!(menus, "choose {timeout}--default DEFAULT target && goto ${{target}} || goto f0").unwrap();
            }
        }
    }
    if default == "local" && config.menu.default != "local" {
        eprintln!("menu: default {:?} is not an ISO in the folder; using the local disk", config.menu.default);
    }
    // The as-is menu: every ISO by its path, booted untouched.
    menus.push_str(":asis\nmenu Boot as-is (unpatched)\nitem f0 ..\n");
    for (n, path) in root.paths().into_iter().enumerate() {
        writeln!(menus, "item a{n} {path}").unwrap();
        writeln!(boots, ":a{n}\nsanboot {base}/as-is/{} || goto failed", url_path(path)).unwrap();
    }
    menus.push_str("choose target && goto ${target} || goto f0\n");
    let menus = menus.replace("--default DEFAULT ", &format!("--default {default} ")).replace("DEFAULT_ITEM", &default_item);
    format!(
        "#!ipxe\ngoto f0\n{menus}{boots}\
         :failed\nprompt Boot failed. Press a key to return to the menu.\ngoto f0\n\
         :reboot\nreboot\n\
         :firmware\nreboot --setup || goto failed\n\
         :shell\nshell\ngoto f0\n\
         :local\n{LOCAL_DISK}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> Config {
        toml::from_str(&format!("[server]\ninterface = \"eth0\"\niso_dir = \"/x\"\n{extra}")).unwrap()
    }

    #[test]
    fn folder_becomes_a_nested_menu() {
        let dir = std::env::temp_dir().join(format!("oxi-menu-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("linux/empty")).unwrap();
        for file in ["b tool.ISO", "a.iso", "notes.txt", ".hidden.iso", "secret.iso", "linux/arch.iso"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let config = config(
            "[menu]\ntimeout = 5\ndefault = \"linux/arch.iso\"\n\
             [iso.\"secret.iso\"]\nhidden = true\n[iso.\"linux/arch.iso\"]\nname = \"Arch Linux\"\n[iso.\"a.iso\"]\nas_is = true\n",
        );
        let root = scan(&dir, "", &config.iso);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(root.isos, [("a".into(), "a.iso".into()), ("b tool".into(), "b tool.ISO".into())]);
        assert_eq!(root.folders.len(), 1);
        assert_eq!(root.folders[0].isos, [("Arch Linux".into(), "linux/arch.iso".into())]);

        let script = script(&root, &config, "http://10.0.0.1");
        assert!(script.starts_with("#!ipxe\ngoto f0\n:f0\nmenu oxi-boot\nitem f1 linux/\nitem i0 a\nitem i1 b tool\nitem i2 Arch Linux\nitem --gap\n")); // the default, from its folder
        assert!(script.contains("choose --timeout 5000 --default i2 target && goto ${target} || goto f0\n"));
        assert!(script.contains(":f1\nmenu linux\nitem f0 ..\nitem i2 Arch Linux\nchoose target"));
        assert!(script.contains("item --gap\nitem asis Boot an ISO as-is (unpatched)...\nitem local "));
        assert!(script.contains(":i0\nsanboot http://10.0.0.1/as-is/a.iso || goto failed\n")); // as_is in the config
        assert!(script.contains(":i1\nchain http://10.0.0.1/boot/b%20tool.ISO || goto failed\n"));
        assert!(script.contains(":i2\nchain http://10.0.0.1/boot/linux/arch.iso || goto failed\n"));
        assert!(script.contains(":asis\nmenu Boot as-is (unpatched)\nitem f0 ..\nitem a0 a.iso\nitem a1 b tool.ISO\nitem a2 linux/arch.iso\nchoose"));
        assert!(script.contains(":a2\nsanboot http://10.0.0.1/as-is/linux/arch.iso || goto failed\n"));
        for label in ["local", "reboot", "firmware", "shell", "failed"] {
            assert!(script.contains(&format!("\n:{label}\n")), "{label}");
        }
    }

    #[test]
    fn timeout_zero_waits_forever() {
        let script = script(&Folder::default(), &config("[menu]\ntimeout = 0\n"), "http://x");
        assert!(script.contains("\nchoose --default local target"));
    }
}
