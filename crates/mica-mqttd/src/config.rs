//! What the bridge is told at startup, and the protocol's fixed timings.

use std::time::Duration;

/// Read an optional private JSON credential file without echoing its contents.
///
/// Absence selects anonymous MQTT. An existing invalid or publicly readable file
/// refuses startup, so a broken credential is never silently ignored.
pub fn broker_credentials(path: &std::path::Path) -> anyhow::Result<Option<(String, String)>> {
    use anyhow::{Context, ensure};
    use std::os::unix::fs::PermissionsExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read broker credentials {}", path.display()));
        }
    };
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "broker credentials {} must be a private regular file (0600)",
        path.display()
    );
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read broker credentials {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("invalid broker credentials JSON in {}", path.display()))?;
    let credentials = value
        .as_object()
        .filter(|fields| fields.len() == 2)
        .and_then(|fields| {
            Some((
                fields.get("username")?.as_str()?,
                fields.get("password")?.as_str()?,
            ))
        })
        .filter(|(username, password)| !username.is_empty() && !password.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "broker credentials {} require nonempty username and password strings",
                path.display()
            )
        })?;
    Ok(Some((credentials.0.to_string(), credentials.1.to_string())))
}

/// Whether application write requests are carried through to the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Mode {
    /// `N` only. `W` topics are refused and never reach `SetValue`, and the
    /// bridge does not even subscribe to them — the refusal is belt and
    /// braces, because a broker that ignores the subscription set, or a
    /// message already in flight when the mode changed, must still be
    /// refused by the code that acts on it.
    #[default]
    ReadOnly,
    /// `W` topics become `SetValue` on the uniquely addressed application
    /// item. Exact package enrollment still applies.
    Full,
}

impl Mode {
    /// Whether a write request may proceed to the bus.
    pub fn writes_allowed(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// The protocol's three time constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// How long one keepalive keeps the bridge publishing. Every publication
    /// is gated on this window: no keepalive, or an expired one, and the
    /// bridge is silent.
    pub alive_window: Duration,
    /// The interval between heartbeat publications while alive.
    pub heartbeat: Duration,
    /// The floor between two full republishes.
    pub full_publish_min_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            alive_window: Duration::from_secs(60),
            heartbeat: Duration::from_secs(3),
            full_publish_min_interval: Duration::from_secs(5),
        }
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn credentials_are_optional_but_existing_files_must_be_private_and_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        assert!(broker_credentials(&path).unwrap().is_none());
        std::fs::write(&path, r#"{"username":"bridge","password":"test-secret"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            broker_credentials(&path).unwrap(),
            Some(("bridge".into(), "test-secret".into()))
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(broker_credentials(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(broker_credentials(&link).is_err());
        for invalid in [
            "test-secret-invalid-json",
            r#"{"username":"bridge","password":["test-secret"]}"#,
            r#"{"username":"","password":"test-secret"}"#,
            r#"{"username":"bridge","password":"test-secret","extra":true}"#,
        ] {
            std::fs::write(&path, invalid).unwrap();
            let error = broker_credentials(&path).unwrap_err();
            assert!(!format!("{error:#}").contains("test-secret"));
        }
    }
}
