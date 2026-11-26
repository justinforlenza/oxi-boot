use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use figment2::Figment;
use figment2::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub dhcp: Dhcp,
    #[serde(default)]
    pub menu: Menu,
    #[serde(default)]
    pub access: Access,
    /// Per-ISO settings, keyed by the ISO's path under `iso_dir`.
    #[serde(default)]
    pub iso: HashMap<String, IsoOverride>,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IsoOverride {
    /// Shown in the menu instead of the file name.
    pub name: Option<String>,
    /// Left out of the menu.
    pub hidden: bool,
    /// Boot the ISO unpatched.
    pub as_is: bool,
    /// The first way the helper tries to attach the ISO; it falls back to
    /// the later ones. Default: nbd.
    pub attach: Option<Attach>,
}

/// Ways to attach an ISO on the client, in fallback order.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Attach {
    /// A network block device.
    Nbd,
    /// A FUSE file under a loop device, for kernels without NBD.
    Fuse,
    /// The whole ISO copied into the client's memory.
    Ram,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// The network card to serve on. Unset: the one the default route uses.
    pub interface: Option<String>,
    pub iso_dir: PathBuf,
    /// Folder holding the iPXE files (undionly.kpxe, snponly.efi) and the
    /// helper program (oxi-boot-helper).
    pub ipxe_dir: PathBuf,
}

impl Default for Server {
    fn default() -> Self {
        // A release download keeps its boot files in `boot`, next to the program.
        let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("boot"))).filter(|dir| dir.is_dir());
        Server { interface: None, iso_dir: "/iso".into(), ipxe_dir: beside.unwrap_or_else(|| "/usr/share/oxi-boot".into()) }
    }
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Dhcp {
    pub mode: DhcpMode,
    /// Address pool, full mode only.
    pub range: Option<String>,
    /// Gateway and name servers handed to clients, full mode only.
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DhcpMode {
    #[default]
    Proxy,
    Full,
    External,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Menu {
    /// Seconds before the default entry boots; 0 waits forever.
    pub timeout: u64,
    pub default: String,
}

impl Default for Menu {
    fn default() -> Self {
        Self {
            timeout: 10,
            default: "local".into(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Access {
    /// What a machine on neither list gets.
    pub policy: Policy,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    #[default]
    Allow,
    Deny,
}

impl Access {
    /// Whether the machine with this MAC address may network boot. The deny
    /// list wins over the allow list; the policy covers everyone else.
    pub fn allows(&self, mac: &str) -> bool {
        let same = |listed: &String| listed.replace('-', ":").eq_ignore_ascii_case(&mac.replace('-', ":"));
        !self.deny.iter().any(same) && (self.allow.iter().any(same) || self.policy == Policy::Allow)
    }
}

pub const USAGE: &str = "usage: oxi-boot [config file] [--section.setting value]...

Every setting can come from the config file (default: oxi-boot.toml), from an
environment variable, or from an argument; each overrides the one before.
  file         [dhcp]  mode = \"full\"
  environment  OXI_BOOT_DHCP__MODE=full
  argument     --dhcp.mode full
A list is written [a,b] outside the file.";

/// The settings from the command line `args`: the config file named there
/// (if it exists), then `OXI_BOOT_` environment variables, then `--key value`
/// arguments. With none of them, every default applies.
pub fn load(args: impl IntoIterator<Item = String>) -> Result<Config, Box<dyn std::error::Error>> {
    let (mut path, mut set) = (None, Vec::new());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.strip_prefix("--") {
            None if path.is_none() => path = Some(arg),
            None => return Err(format!("unexpected argument {arg:?}\n{USAGE}").into()),
            Some(key) => {
                let (key, value) = match key.split_once('=') {
                    Some((key, value)) => (key.to_string(), value.to_string()),
                    None => (key.to_string(), args.next().ok_or(format!("--{key} needs a value"))?),
                };
                set.push((key, value));
            }
        }
    }
    let path = path.unwrap_or_else(|| "oxi-boot.toml".into());
    if !std::path::Path::new(&path).exists() {
        eprintln!("oxi-boot: no config file at {path}");
    }
    // `__` separates section and setting, because setting names have `_` in them.
    let mut figment = Figment::new().merge(Toml::file(&path).search(false)).merge(Env::prefixed("OXI_BOOT_").split("__"));
    for (key, value) in set {
        // Read like an environment variable's value: 10 is a number, [a,b] a list.
        figment = figment.merge((key, value.parse::<figment2::value::Value>()?));
    }
    Ok(figment.extract()?)
}

/// A config from TOML text, for tests.
#[cfg(test)]
pub fn parse(text: &str) -> Result<Config, Box<figment2::Error>> {
    Figment::new().merge(Toml::string(text)).extract().map_err(Box::new)
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn example_file_parses() {
        let config = parse(include_str!("../../../oxi-boot.example.toml")).unwrap();
        assert_eq!(
            config,
            Config {
                server: Server {
                    interface: Some("eth0".into()),
                    iso_dir: "/srv/iso".into(),
                    ipxe_dir: "/usr/share/oxi-boot".into(),
                },
                dhcp: Dhcp {
                    mode: DhcpMode::Proxy,
                    range: Some("192.168.1.100-192.168.1.200".into()),
                    router: None,
                    dns: vec![],
                },
                menu: Menu::default(),
                access: Access {
                    policy: Policy::Allow,
                    allow: vec![],
                    deny: vec!["aa:bb:cc:dd:ee:ff".into()],
                },
                iso: HashMap::new(),
            }
        );
    }

    #[test]
    fn arguments_override_the_file() {
        let file = std::env::temp_dir().join(format!("oxi-boot-test-{}.toml", std::process::id()));
        std::fs::write(&file, "[dhcp]\nmode = \"full\"\n[menu]\ntimeout = 5\n[iso.\"Linux/Debian-13.iso\"]\nhidden = true\n").unwrap();
        let args = [file.to_str().unwrap(), "--menu.timeout", "0", "--dhcp.dns=[1.1.1.1,9.9.9.9]", "--server.iso_dir", "/srv/iso"];
        let config = load(args.map(String::from)).unwrap();
        std::fs::remove_file(file).unwrap();
        assert_eq!(config.dhcp.mode, DhcpMode::Full); // from the file
        assert_eq!(config.menu.timeout, 0); // the argument wins
        assert_eq!(config.dhcp.dns, ["1.1.1.1".parse::<Ipv4Addr>().unwrap(), "9.9.9.9".parse().unwrap()]);
        assert_eq!(config.server.iso_dir, PathBuf::from("/srv/iso"));
        assert!(config.iso["Linux/Debian-13.iso"].hidden); // names keep their capitals
        assert!(load(["--dhcp.mod".to_string(), "full".to_string()]).is_err()); // a misspelt setting
        assert!(load(["--dhcp.mode".to_string()]).is_err());
    }

    #[test]
    fn nothing_is_required() {
        let config = parse("").unwrap();
        assert_eq!(config.server, Server { interface: None, iso_dir: "/iso".into(), ipxe_dir: "/usr/share/oxi-boot".into() });
        assert_eq!(config.dhcp.mode, DhcpMode::Proxy);
    }

    #[test]
    fn only_server_section_is_required() {
        let config = parse("[server]\ninterface = \"eth0\"\niso_dir = \"/srv/iso\"").unwrap();
        assert_eq!(config.dhcp.mode, DhcpMode::Proxy);
        assert_eq!(config.menu.timeout, 10);
    }

    #[test]
    fn access_lists() {
        let access = |policy, allow: &[&str], deny: &[&str]| Access {
            policy,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        };
        let mac = "52:54:00:12:34:56";
        assert!(access(Policy::Allow, &[], &[]).allows(mac));
        assert!(!access(Policy::Allow, &[], &["52-54-00-12-34-56"]).allows(mac));
        assert!(!access(Policy::Deny, &[], &[]).allows(mac));
        assert!(access(Policy::Deny, &["52:54:00:12:34:56"], &[]).allows("52:54:00:12:34:56"));
        assert!(access(Policy::Deny, &["52:54:00:12:34:56"], &[]).allows("52:54:00:12:34:56".to_uppercase().as_str()));
        assert!(!access(Policy::Deny, &[mac], &[mac]).allows(mac)); // deny wins
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(parse("[server]\ninterface = \"eth0\"\niso_dir = \"/x\"\ntypo = 1").is_err());
    }
}
