//! The boot menu: the ISO folder as an iPXE script.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::config::{Builtin, Colours, Config, IsoOverride};

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

/// A section heading: a rule across the menu with `name` in the middle.
fn rule(name: &str) -> String {
    format!("item --gap -- {:-^74}\n", format!(" {name} "))
}

/// A header line as iPXE keeps it. iPXE drops a space after another space or
/// at the start of the line, and reads ` #` as the start of a comment; those
/// spaces become no-break spaces. The rest stay, as a no-break space is two bytes.
fn spaced(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        let droppable = matches!(out.chars().last(), None | Some(' ')) || chars.peek() == Some(&'#');
        out.push(if c == ' ' && droppable { '\u{a0}' } else { c });
    }
    out
}

/// `text` as menu lines, centred as a block.
fn header(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let width = lines.iter().map(|line| line.chars().count()).max().unwrap_or(0);
    // shortcut: assumes an 80-column screen (entries start at column 3), iPXE does
    // not tell a script the real width; wider consoles show the header left of centre.
    let mut pad = (80usize.saturating_sub(width) / 2).saturating_sub(3);
    // iPXE cuts a line at 74 bytes and a no-break space is two, so a wide header
    // with many spaces moves left until it fits.
    let padded = |pad: usize, line: &str| spaced(&format!("{}{line}", " ".repeat(pad)));
    while pad > 0 && lines.iter().any(|line| padded(pad, line).len() > 74) {
        pad -= 1;
    }
    let mut out = String::new();
    for line in &lines {
        match line.is_empty() {
            true => out.push_str("item --gap\n"),
            false => writeln!(out, "item --gap -- {}", padded(pad, line)).unwrap(),
        }
    }
    if !out.is_empty() {
        out.push_str("item --gap\n");
    }
    out
}

/// The whole menu. `base` is the server's URL, without a trailing slash.
pub fn script(root: &Folder, config: &Config, base: &str) -> String {
    // Folders become labels f0, f1, ... (f0 is the top menu) and ISOs i0, i1, ...
    let (mut menus, mut boots, mut default) = (String::new(), String::new(), "local".to_string());
    let mut todo = vec![(root, "f0".to_string(), None::<String>)];
    let (mut folders, mut isos, mut default_item) = (1, 0, String::new());
    while let Some((folder, label, parent)) = todo.pop() {
        let title = if parent.is_some() { folder.name.as_str() } else { config.menu.title.as_str() };
        writeln!(menus, ":{label}\n{}", format!("menu {title}").trim_end()).unwrap();
        match &parent {
            Some(parent) => writeln!(menus, "item {parent} ..").unwrap(),
            None => {
                menus.push_str(&header(&config.menu.header));
                if !root.paths().is_empty() {
                    menus.push_str(&rule("ISOs"));
                }
            }
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
                let builtins: String = [
                    (Builtin::AsIs, "item asis Boot an ISO as-is (unpatched)...\n"),
                    (Builtin::Local, "item local Boot from local disk\n"),
                    (Builtin::Reboot, "item reboot Reboot\n"),
                    (Builtin::Firmware, "item firmware Firmware setup\n"),
                    (Builtin::Shell, "item shell iPXE shell\n"),
                ]
                .into_iter()
                .filter(|(entry, _)| !config.menu.hide.contains(entry) && (*entry != Builtin::AsIs || !root.paths().is_empty()))
                .map(|(_, item)| item)
                .collect();
                if !builtins.is_empty() {
                    menus.push_str("item --gap\n");
                    menus.push_str(&rule("Tools"));
                    menus.push_str(&builtins);
                }
                // The default may sit in a folder not visited yet; filled in below.
                menus.push_str("CHOOSE target && goto ${target} || goto f0\n");
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
    let choose = match (config.menu.timeout, default == "local" && config.menu.hide.contains(&Builtin::Local)) {
        // The default is the local disk and its entry is hidden: nothing to time out to.
        (_, true) => "choose".to_string(),
        (0, _) => format!("choose --default {default}"),
        (seconds, _) => format!("choose --timeout {} --default {default}", seconds * 1000),
    };
    let menus = menus.replace("CHOOSE target", &format!("{choose} target")).replace("DEFAULT_ITEM", &default_item);
    let mut colours = String::new();
    let Colours { text, background, heading, selected_text, selected_background } = config.menu.colours;
    // Pairs 1 to 3 are the menu's text, its selected entry and its headings.
    // An iPXE built without the command keeps its own colours.
    for (pair, fg, bg) in [(1, text, background), (2, selected_text, selected_background), (3, heading, background)] {
        writeln!(colours, "cpair --foreground {} --background {} {pair} ||", fg as u8, bg as u8).unwrap();
    }
    format!(
        "#!ipxe\n{colours}goto f0\n{menus}{boots}\
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

    /// The default colours: black on white, blue headings and selection.
    const COLOURS: &str = "cpair --foreground 0 --background 7 1 ||\ncpair --foreground 7 --background 4 2 ||\ncpair --foreground 4 --background 7 3 ||\n";

    fn config(extra: &str) -> Config {
        crate::config::parse(&format!("[server]\ninterface = \"eth0\"\niso_dir = \"/x\"\n{extra}")).unwrap()
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
        assert!(script.starts_with(&format!("#!ipxe\n{COLOURS}goto f0\n:f0\nmenu\n{}{}item f1 linux/\nitem i0 a\nitem i1 b tool\nitem i2 Arch Linux\nitem --gap\n", header(&config.menu.header), rule("ISOs")))); // the default, from its folder
        assert!(script.contains("choose --timeout 5000 --default i2 target && goto ${target} || goto f0\n"));
        assert!(script.contains(":f1\nmenu linux\nitem f0 ..\nitem i2 Arch Linux\nchoose target"));
        assert!(script.contains(&format!("item --gap\n{}item asis Boot an ISO as-is (unpatched)...\nitem local ", rule("Tools"))));
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
    fn title_hidden_entries_and_colours() {
        let config = config(
            "[menu]\ntitle = \"Lab\"\nheader = \"\"\"\n/\\\\ /\\\\\n\n a  b\n\"\"\"\nhide = [\"local\", \"shell\"]\n\
             [menu.colours]\ntext = \"green\"\nbackground = \"black\"\nselected_text = \"yellow\"\n",
        );
        let script = script(&Folder::default(), &config, "http://x");
        // The header is 5 wide: (80 - 5) / 2 - 3 = 34 columns of padding put it in the middle.
        let pad = "\u{a0} ".repeat(17);
        assert!(script.starts_with(&format!(
            "#!ipxe\ncpair --foreground 2 --background 0 1 ||\ncpair --foreground 3 --background 4 2 ||\ncpair --foreground 4 --background 0 3 ||\n\
             goto f0\n:f0\nmenu Lab\nitem --gap -- {pad}/\\ /\\\nitem --gap\nitem --gap -- {pad}\u{a0}a \u{a0}b\nitem --gap\n\
             item --gap\n{}item reboot Reboot\nitem firmware Firmware setup\nchoose target && goto ${{target}} || goto f0\n",
            rule("Tools")
        )));
        assert_eq!(spaced("   # #  #"), "\u{a0} \u{a0}#\u{a0}# \u{a0}#");
        // Too wide to centre in 74 bytes: it moves left, and every line still fits.
        let wide = header(&format!("{}\n", "#    ".repeat(10)));
        assert!(wide.starts_with("item --gap -- \u{a0}") && wide.lines().all(|line| line.len() <= "item --gap -- ".len() + 74));
        assert_eq!(rule("Tools").trim_end().chars().count(), "item --gap -- ".len() + 74);
        let all = config_hide_all();
        assert!(super::script(&Folder::default(), &all, "http://x").starts_with(&format!("#!ipxe\n{COLOURS}goto f0\n:f0\nmenu\n{}choose target", header(&all.menu.header))));
        // The default header is the logo, centred: 32 wide, so 21 columns of padding before its leading space.
        assert!(header(&all.menu.header).starts_with(&format!("item --gap -- {}__", "\u{a0} ".repeat(11))));
    }

    fn config_hide_all() -> Config {
        config("[menu]\nhide = [\"as_is\", \"local\", \"reboot\", \"firmware\", \"shell\"]\n")
    }

    #[test]
    fn timeout_zero_waits_forever() {
        let script = script(&Folder::default(), &config("[menu]\ntimeout = 0\n"), "http://x");
        assert!(script.contains("\nchoose --default local target"));
    }
}
