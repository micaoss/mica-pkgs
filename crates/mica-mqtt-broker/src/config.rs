//! The mica-owned broker configuration, and the credentials file beside it.
//!
//! Neither format is rumqttd's. rumqttd reads a large TOML of its own with a
//! router section, per-listener sections and a console block; none of that is
//! a decision anyone operating a device should have to make. What micad renders
//! into `/var/lib/mica/mqtt/broker.toml` is three keys, and this module is the
//! only thing that reads them. The rumqttd `Config` is then built programmatically
//! in `main`, so the values that are not configurable stay out of reach.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The whole of the mica-owned broker configuration.
///
/// All three keys are required. micad renders this file from the `mqtt`
/// settings subtree immediately before it starts the unit, so a missing key
/// means the renderer is broken rather than the operator being terse, and
/// guessing a default would hide that.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BrokerConfig {
    /// The address to bind. An IP address, not a hostname: this is a bind, and
    /// a bind names an interface rather than resolving one.
    pub listen_address: String,
    /// The TCP port to bind. One port, and therefore one listener: see the
    /// note on `broker_config` in `main.rs` for why MQTT 5.0 cannot simply be
    /// served alongside 3.1.1 on it.
    pub listen_port: u16,
    /// Whether connections must present a username and password from
    /// [`load_users`].
    pub auth_enabled: bool,
}

impl BrokerConfig {
    /// The listen address, parsed.
    ///
    /// Split from [`parse`] so that a bad address is reported against the
    /// file it came from rather than as a serde field error.
    pub fn address(&self) -> Result<IpAddr> {
        self.listen_address.parse::<IpAddr>().with_context(|| {
            format!(
                "listen_address {:?} is not an IP address (the broker binds an \
                 interface, it does not resolve a name)",
                self.listen_address
            )
        })
    }
}

/// Parse the mica-owned configuration from TOML text.
pub fn parse(text: &str) -> Result<BrokerConfig> {
    Ok(toml::from_str(text)?)
}

/// Read and parse the mica-owned configuration.
///
/// Every error names the path. This binary is started by systemd with no
/// arguments an operator ever sees, so the journal line is the only place the
/// path can appear.
pub fn load(path: &Path) -> Result<BrokerConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading broker config {}", path.display()))?;
    parse(&text).with_context(|| format!("parsing broker config {}", path.display()))
}

/// The credentials file: a single `[users]` table of name -> password.
#[derive(Debug, Default, Deserialize)]
struct UsersFile {
    #[serde(default)]
    users: HashMap<String, String>,
}

/// Parse the credentials file from TOML text.
pub fn parse_users(text: &str) -> Result<HashMap<String, String>> {
    Ok(toml::from_str::<UsersFile>(text)?.users)
}

/// Read the credentials file, treating an absent one as no users at all.
pub fn load_users(path: &Path) -> Result<HashMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_users(&text).with_context(|| format!("parsing {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether the broker would accept connections from off this device without
/// asking for a password.
pub fn is_off_host_unauthenticated(address: IpAddr, auth_enabled: bool) -> bool {
    !address.is_loopback() && !auth_enabled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn parses_the_rendered_config() {
        let cfg = parse(
            r#"
listen_address = "127.0.0.1"
listen_port = 1883
auth_enabled = false
"#,
        )
        .expect("the shape micad renders must parse");
        assert_eq!(
            cfg,
            BrokerConfig {
                listen_address: "127.0.0.1".to_string(),
                listen_port: 1883,
                auth_enabled: false,
            }
        );
        assert_eq!(cfg.address().unwrap(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn a_missing_key_is_an_error_naming_it() {
        let err = parse(
            r#"
listen_address = "127.0.0.1"
auth_enabled = false
"#,
        )
        .expect_err("listen_port is required");
        assert!(
            format!("{err:#}").contains("listen_port"),
            "the error must name the missing key, got: {err:#}"
        );
    }

    #[test]
    fn a_hostname_is_not_a_bind_address() {
        let cfg = parse(
            r#"
listen_address = "localhost"
listen_port = 1883
auth_enabled = false
"#,
        )
        .expect("the file itself is well formed");
        let err = cfg.address().expect_err("a name is not an address");
        assert!(format!("{err:#}").contains("localhost"));
    }

    #[test]
    fn parses_the_users_table() {
        let users = parse_users(
            r#"
[users]
venus = "hunter2"
grafana = "s3cret"
"#,
        )
        .expect("a well formed users file must parse");
        assert_eq!(users.len(), 2);
        assert_eq!(users.get("venus").map(String::as_str), Some("hunter2"));
    }

    #[test]
    fn an_absent_users_file_is_no_users_rather_than_an_error() {
        // A path under the temp directory that nothing ever creates, made
        // unique per process so a concurrent run cannot make this flake.
        let path = std::env::temp_dir()
            .join(format!("mica-mqtt-broker-absent-{}", std::process::id()))
            .join("mqtt-broker-users.toml");
        assert!(!path.exists(), "the test must not read a real file");
        let users = load_users(&path).expect("an absent credentials file must not fail");
        assert!(users.is_empty());
    }

    /// Every shape micad renders (its `render_config`: three lines, a quoted
    /// literal address) reads back as those values.
    #[test]
    fn every_rendered_shape_parses() {
        let cases = [
            ("127.0.0.1", 1883, false),
            ("0.0.0.0", 1883, true),
            ("10.0.0.5", 8883, true),
            ("::1", 1, false),
            ("::", 65535, true),
            ("fe80::1", 1883, true),
            ("localhost", 1883, true),
        ];
        for (address, port, auth) in cases {
            let text = format!(
                "listen_address = \"{address}\"\nlisten_port = {port}\nauth_enabled = {auth}\n"
            );
            assert_eq!(
                parse(&text).unwrap(),
                BrokerConfig {
                    listen_address: address.to_string(),
                    listen_port: port,
                    auth_enabled: auth,
                },
                "{text}"
            );
        }
    }

    /// What the parser refuses: a wrong type, a port out of range, a repeated
    /// key, and text that is not TOML.
    #[test]
    fn malformed_configs_are_refused() {
        for text in [
            "listen_address = \"127.0.0.1\"\nlisten_port = \"1883\"\nauth_enabled = false\n",
            "listen_address = \"127.0.0.1\"\nlisten_port = 65536\nauth_enabled = false\n",
            "listen_address = \"127.0.0.1\"\nlisten_port = -1\nauth_enabled = false\n",
            "listen_address = \"127.0.0.1\"\nlisten_port = 1883\nauth_enabled = 0\n",
            "listen_address = \"a\"\nlisten_address = \"b\"\nlisten_port = 1\nauth_enabled = true\n",
            "listen_address = 127.0.0.1\nlisten_port = 1883\nauth_enabled = false\n",
            "",
        ] {
            assert!(parse(text).is_err(), "{text:?} must be refused");
        }
    }

    /// Hand-written credentials files: comments, quoted and dotted-looking
    /// keys, escapes, literal strings, and an empty table.
    #[test]
    fn credentials_files_parse_as_written() {
        let users = parse_users(
            "# accounts\n[users]\n\"user.name\" = \"p\\\"w\\\\d\"\n'lit' = 'a\\b'\nplain = \"\" # empty\n\"\u{e9}t\u{e9}\" = \"\\u00e9\"\n",
        )
        .unwrap();
        let mut got: Vec<(&str, &str)> = users
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            [
                ("lit", "a\\b"),
                ("plain", ""),
                ("user.name", "p\"w\\d"),
                ("\u{e9}t\u{e9}", "\u{e9}")
            ]
        );
        assert!(parse_users("[users]\n").unwrap().is_empty());
        for text in [
            "[users]\nname = 1\n",
            "[users]\na = \"x\"\na = \"y\"\n",
            "users = \"x\"\n",
        ] {
            assert!(parse_users(text).is_err(), "{text:?} must be refused");
        }
    }

    #[test]
    fn a_users_file_with_no_table_is_no_users() {
        assert!(parse_users("# nobody enrolled yet\n").unwrap().is_empty());
    }

    #[test]
    fn only_off_host_without_auth_warns() {
        let loopback_v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let loopback_v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let any_v4 = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let any_v6 = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));

        assert!(is_off_host_unauthenticated(any_v4, false));
        assert!(is_off_host_unauthenticated(any_v6, false));
        assert!(is_off_host_unauthenticated(lan, false));

        // Auth on: off-host is a deliberate, defended configuration.
        assert!(!is_off_host_unauthenticated(lan, true));
        // Loopback: nothing off the device can reach it either way.
        assert!(!is_off_host_unauthenticated(loopback_v4, false));
        assert!(!is_off_host_unauthenticated(loopback_v6, false));
    }
}
