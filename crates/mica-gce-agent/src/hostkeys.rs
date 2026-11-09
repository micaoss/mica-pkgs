//! The instance's SSH host keys as guest attributes, so gcloud and the console
//! can check the host key instead of trusting it on first use.
//!
//! As Google's guest agent does, each public key goes to
//! `instance/guest-attributes/hostkeys/<type>`, the value being the key's
//! base64 field alone. Only the `.pub` files are read, never a private key,
//! and only ed25519 and RSA keys are sent. A 403 or 404 means guest
//! attributes are off: logged once, and tried again only when the metadata's
//! `enable-guest-attributes` changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::metadata::{Client, Put};

/// The host keys mica-openssh makes, by file and the type they must hold.
const KEYS: &[(&str, &str)] = &[
    ("ssh_host_ed25519_key.pub", "ssh-ed25519"),
    ("ssh_host_rsa_key.pub", "ssh-rsa"),
];

/// The guest attribute path for one key type.
const PREFIX: &str = "/computeMetadata/v1/instance/guest-attributes/hostkeys/";

fn base64(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

/// The public key of `type_` in `text`: its base64 field, if the first line
/// is `<type_> <base64> [comment]`.
fn public_key(text: &str, type_: &str) -> Option<String> {
    let mut fields = text.lines().next()?.split_whitespace();
    (fields.next()? == type_)
        .then(|| fields.next())
        .flatten()
        .filter(|blob| base64(blob))
        .map(str::to_string)
}

/// The host public keys in `dir`, type -> base64. A missing file is no key;
/// a file that holds anything else is logged and skipped.
pub fn read(dir: &Path) -> BTreeMap<String, String> {
    let mut keys = BTreeMap::new();
    for (file, type_) in KEYS {
        let path = dir.join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::debug!("no host key {}", path.display());
            continue;
        };
        match public_key(&text, type_) {
            Some(blob) => {
                keys.insert((*type_).to_string(), blob);
            }
            None => tracing::warn!("{} is not one {type_} public key", path.display()),
        }
    }
    keys
}

/// The metadata's `enable-guest-attributes`, instance and project.
fn switch(document: &Value) -> [Option<String>; 2] {
    ["instance", "project"].map(|level| {
        document
            .pointer(&format!("/{level}/attributes/enable-guest-attributes"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// What has been published, across passes.
#[derive(Debug)]
pub struct Publisher {
    pub dir: PathBuf,
    published: BTreeMap<String, String>,
    /// `enable-guest-attributes` at the last pass; a change publishes again.
    switch: [Option<String>; 2],
    /// Guest attributes answered 403 or 404 at this switch.
    disabled: bool,
    /// The last pass failed; the next failure is not logged again.
    failing: bool,
}

impl Publisher {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            published: BTreeMap::new(),
            switch: [None, None],
            disabled: false,
            failing: false,
        }
    }

    /// Publish the keys that differ from the last pass, once per pass.
    pub async fn pass(&mut self, client: &Client, document: &Value) {
        let switch = switch(document);
        if switch != self.switch {
            self.switch = switch;
            self.published.clear();
            self.disabled = false;
        }
        if self.disabled {
            return;
        }
        for (type_, blob) in read(&self.dir) {
            if self.published.get(&type_) == Some(&blob) {
                continue;
            }
            match client.put(&format!("{PREFIX}{type_}"), &blob).await {
                Ok(Put::Done) => {
                    tracing::info!(key = type_, "host key published");
                    self.failing = false;
                    self.published.insert(type_, blob);
                }
                Ok(Put::Off) => {
                    tracing::info!("guest attributes disabled; host keys not published");
                    self.disabled = true;
                    return;
                }
                Err(err) => {
                    if !self.failing {
                        tracing::warn!("publishing the host keys: {err:#}; retrying next pass");
                    }
                    self.failing = true;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_public_key_of_the_named_type_reads() {
        assert_eq!(
            public_key("ssh-ed25519 AAAAC3+/= root@host\n", "ssh-ed25519").as_deref(),
            Some("AAAAC3+/=")
        );
        assert_eq!(
            public_key("ssh-rsa AAAAB3", "ssh-rsa").as_deref(),
            Some("AAAAB3")
        );
        for text in [
            "",
            "ssh-ed25519\n",
            "ssh-rsa AAAAB3\n",
            "ssh-ed25519 AA&A\n",
            "ecdsa-sha2-nistp256 AAAAE2\n",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n",
        ] {
            assert_eq!(public_key(text, "ssh-ed25519"), None, "{text:?}");
        }
    }

    #[test]
    fn the_two_pub_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_empty());
        std::fs::write(
            dir.path().join("ssh_host_ed25519_key.pub"),
            "ssh-ed25519 AAAA\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ssh_host_rsa_key.pub"),
            "ssh-ed25519 BBBB\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("ssh_host_ed25519_key"), "secret").unwrap();
        assert_eq!(
            read(dir.path()),
            BTreeMap::from([("ssh-ed25519".to_string(), "AAAA".to_string())])
        );
    }

    #[test]
    fn the_switch_is_read_at_both_levels() {
        assert_eq!(switch(&json!({})), [None, None]);
        assert_eq!(
            switch(&json!({
                "instance": {"attributes": {"enable-guest-attributes": "TRUE"}},
                "project": {"attributes": {"enable-guest-attributes": "false"}},
            })),
            [Some("TRUE".to_string()), Some("false".to_string())]
        );
    }
}
