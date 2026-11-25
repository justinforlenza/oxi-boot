use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
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
#[serde(deny_unknown_fields)]
pub struct Server {
    pub interface: String,
    pub iso_dir: PathBuf,
    /// Folder holding the iPXE files (undionly.kpxe, snponly.efi) and the
    /// helper program (oxi-boot-helper).
    #[serde(default = "default_ipxe_dir")]
    pub ipxe_dir: PathBuf,
}

fn default_ipxe_dir() -> PathBuf {
    "/usr/share/oxi-boot".into()
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

pub fn load(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_file_parses() {
        let config: Config = toml::from_str(include_str!("../../../oxi-boot.example.toml")).unwrap();
        assert_eq!(
            config,
            Config {
                server: Server {
                    interface: "eth0".into(),
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
    fn only_server_section_is_required() {
        let config: Config = toml::from_str("[server]\ninterface = \"eth0\"\niso_dir = \"/srv/iso\"").unwrap();
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
        assert!(toml::from_str::<Config>("[server]\ninterface = \"eth0\"\niso_dir = \"/x\"\ntypo = 1").is_err());
    }
}
