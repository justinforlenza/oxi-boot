// Fields are only printed until the server modules that read them exist.
#![allow(dead_code)]

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
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub interface: String,
    pub iso_dir: PathBuf,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Dhcp {
    pub mode: DhcpMode,
    /// Address pool, full mode only.
    pub range: Option<String>,
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
                },
                dhcp: Dhcp {
                    mode: DhcpMode::Proxy,
                    range: Some("192.168.1.100-192.168.1.200".into()),
                },
                menu: Menu::default(),
                access: Access {
                    policy: Policy::Allow,
                    allow: vec![],
                    deny: vec!["aa:bb:cc:dd:ee:ff".into()],
                },
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
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[server]\ninterface = \"eth0\"\niso_dir = \"/x\"\ntypo = 1").is_err());
    }
}
