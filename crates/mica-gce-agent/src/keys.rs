//! Which users get which SSH keys, read from the GCE metadata document.
//!
//! The keys are the `ssh-keys` attributes of the instance and the project (and
//! the older `sshKeys`), one `<user>:<type> <base64> [<comment>]` per line.
//! The project's keys count unless the instance sets `block-project-ssh-keys`
//! or carries `sshKeys`. A comment `google-ssh {"userName":..,"expireOn":..}`,
//! which gcloud and the console write, ends the key at `expireOn`. With
//! `enable-oslogin` set, on the instance or else on the project, no metadata
//! key counts.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// The key types sshd accepts in an authorized_keys line. Anything else,
/// including an options prefix such as `command="..."`, is refused.
const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// The comment prefix of a key gcloud or the console added.
const GOOGLE_SSH: &str = "google-ssh ";

/// What the metadata asks for at one moment.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Each user's authorized_keys lines, in metadata order, without duplicates.
    pub users: BTreeMap<String, Vec<String>>,
    /// When the earliest key still in `users` expires.
    pub next_expiry: Option<DateTime<Utc>>,
    /// OS Login is on, so no metadata key is applied.
    pub oslogin: bool,
    /// Lines that were refused, with the reason, for the log.
    pub refused: Vec<String>,
}

/// One accepted key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub user: String,
    /// `<type> <base64>[ <comment>]`, as written to authorized_keys.
    pub line: String,
    pub expires: Option<DateTime<Utc>>,
}

/// A user name the agent will create, mica-accounts' login name rule:
/// `[a-z_][a-z0-9_-]{0,31}`.
pub fn valid_user(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c))
}

/// A metadata attribute as a boolean, as Google's agent reads them.
fn truthy(value: Option<&Value>) -> Option<bool> {
    let text = value?.as_str()?.trim().to_ascii_lowercase();
    Some(matches!(text.as_str(), "true" | "1" | "yes" | "y" | "on"))
}

fn attribute<'a>(metadata: &'a Value, scope: &str, name: &str) -> Option<&'a Value> {
    metadata.get(scope)?.get("attributes")?.get(name)
}

/// Parse one metadata line. `Ok(None)` is a blank line.
pub fn parse_line(line: &str) -> Result<Option<Key>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    if line.chars().any(char::is_control) {
        return Err("a control character".into());
    }
    let (user, rest) = line.split_once(':').ok_or("no `<user>:`")?;
    if !valid_user(user) {
        return Err(format!(
            "user name `{user}` is not [a-z_][a-z0-9_-]{{0,31}}"
        ));
    }
    let mut fields = rest.split_whitespace();
    let kind = fields.next().ok_or("no key type")?;
    if !KEY_TYPES.contains(&kind) {
        return Err(format!("key type `{kind}` is not accepted"));
    }
    let blob = fields.next().ok_or("no key")?;
    if !blob
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c))
    {
        return Err("the key is not base64".into());
    }
    let comment = fields.collect::<Vec<_>>().join(" ");
    let expires = match comment.strip_prefix(GOOGLE_SSH) {
        Some(json) => expire_on(json)?,
        None => None,
    };
    let mut written = format!("{kind} {blob}");
    if !comment.is_empty() {
        written.push(' ');
        written.push_str(&comment);
    }
    Ok(Some(Key {
        user: user.to_string(),
        line: written,
        expires,
    }))
}

/// The `expireOn` of a google-ssh comment. gcloud writes `+0000`; RFC 3339
/// offsets are read too. A comment that is not this JSON refuses the key.
fn expire_on(json: &str) -> Result<Option<DateTime<Utc>>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|err| format!("google-ssh comment: {err}"))?;
    let Some(text) = value.get("expireOn") else {
        return Ok(None);
    };
    let text = text.as_str().ok_or("google-ssh expireOn is not a string")?;
    DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z")
        .or_else(|_| DateTime::parse_from_rfc3339(text))
        .map(|time| Some(time.with_timezone(&Utc)))
        .map_err(|err| format!("google-ssh expireOn `{text}`: {err}"))
}

/// The keys and users the metadata document asks for at `now`.
pub fn desired(metadata: &Value, now: DateTime<Utc>) -> Desired {
    let mut out = Desired {
        oslogin: truthy(attribute(metadata, "instance", "enable-oslogin"))
            .or_else(|| truthy(attribute(metadata, "project", "enable-oslogin")))
            .unwrap_or(false),
        ..Desired::default()
    };
    if out.oslogin {
        return out;
    }
    let legacy = attribute(metadata, "instance", "sshKeys").is_some();
    let block = legacy
        || truthy(attribute(metadata, "instance", "block-project-ssh-keys")).unwrap_or(false);
    let mut sources = vec![("instance", "ssh-keys"), ("instance", "sshKeys")];
    if !block {
        sources.extend([("project", "ssh-keys"), ("project", "sshKeys")]);
    }
    for (scope, name) in sources {
        let Some(text) = attribute(metadata, scope, name).and_then(Value::as_str) else {
            continue;
        };
        for line in text.lines() {
            match parse_line(line) {
                Ok(None) => {}
                Ok(Some(key)) if key.expires.is_some_and(|at| at <= now) => {}
                Ok(Some(key)) => {
                    if let Some(at) = key.expires {
                        out.next_expiry = Some(out.next_expiry.map_or(at, |next| next.min(at)));
                    }
                    let lines = out.users.entry(key.user).or_default();
                    if !lines.contains(&key.line) {
                        lines.push(key.line);
                    }
                }
                Err(reason) => out.refused.push(format!("{scope} {name}: {reason}")),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtest";
    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQtest";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn meta(instance: Value, project: Value) -> Value {
        json!({"instance": {"attributes": instance}, "project": {"attributes": project}})
    }

    #[test]
    fn a_plain_line_keeps_its_comment() {
        let key = parse_line(&format!("alice:{ED} alice@laptop"))
            .unwrap()
            .unwrap();
        assert_eq!(key.user, "alice");
        assert_eq!(key.line, format!("{ED} alice@laptop"));
        assert_eq!(key.expires, None);
    }

    #[test]
    fn a_google_ssh_comment_carries_the_expiry() {
        let line = format!(
            r#"bob:{RSA} google-ssh {{"userName":"bob@example.com","expireOn":"2026-11-08T12:00:00+0000"}}"#
        );
        let key = parse_line(&line).unwrap().unwrap();
        assert_eq!(key.expires, Some(at("2026-11-08T12:00:00Z")));
        let rfc = format!(r#"bob:{RSA} google-ssh {{"expireOn":"2026-11-08T12:00:00.5+01:00"}}"#);
        assert!(parse_line(&rfc).unwrap().unwrap().expires.is_some());
        let none = format!(r#"bob:{RSA} google-ssh {{"userName":"bob@example.com"}}"#);
        assert_eq!(parse_line(&none).unwrap().unwrap().expires, None);
    }

    #[test]
    fn bad_lines_are_refused() {
        for line in [
            ED.to_string(),
            format!("Alice:{ED}"),
            format!("root user:{ED}"),
            format!("a.b:{ED}"),
            format!("{}:{ED}", "a".repeat(33)),
            "alice:ssh-dss AAAA".to_string(),
            format!("alice:command=\"sh\" {ED}"),
            "alice:ssh-ed25519".to_string(),
            "alice:ssh-ed25519 AAAA$bad".to_string(),
            format!("alice:{ED} a\u{7}b"),
            format!(r#"alice:{ED} google-ssh {{"expireOn":"tomorrow"}}"#),
            format!("alice:{ED} google-ssh not-json"),
        ] {
            assert!(parse_line(&line).is_err(), "{line}");
        }
        assert_eq!(parse_line("   ").unwrap(), None);
    }

    #[test]
    fn instance_and_project_keys_merge_per_user() {
        let m = meta(
            json!({"ssh-keys": format!("alice:{ED}\nbob:{RSA}\n\nalice:{ED}")}),
            json!({"ssh-keys": format!("alice:{RSA}\ncarol:{ED}")}),
        );
        let d = desired(&m, at("2026-01-01T00:00:00Z"));
        assert_eq!(d.users["alice"], vec![ED.to_string(), RSA.to_string()]);
        assert_eq!(d.users["bob"], vec![RSA.to_string()]);
        assert_eq!(d.users["carol"], vec![ED.to_string()]);
        assert!(d.refused.is_empty());
        assert!(!d.oslogin);
    }

    #[test]
    fn block_project_ssh_keys_and_legacy_sshkeys_drop_the_project() {
        let project = json!({"ssh-keys": format!("carol:{ED}")});
        for instance in [
            json!({"ssh-keys": format!("alice:{ED}"), "block-project-ssh-keys": "TRUE"}),
            json!({"sshKeys": format!("alice:{ED}")}),
        ] {
            let d = desired(&meta(instance, project.clone()), Utc::now());
            assert_eq!(d.users.keys().collect::<Vec<_>>(), ["alice"]);
        }
        let open = json!({"ssh-keys": format!("alice:{ED}"), "block-project-ssh-keys": "false"});
        let d = desired(&meta(open, project), Utc::now());
        assert_eq!(d.users.keys().collect::<Vec<_>>(), ["alice", "carol"]);
    }

    #[test]
    fn expired_keys_drop_and_the_next_expiry_is_the_earliest() {
        let key = |user: &str, when: &str| {
            format!(r#"{user}:{ED} google-ssh {{"userName":"x","expireOn":"{when}"}}"#)
        };
        let lines = [
            key("old", "2026-01-01T00:00:00+0000"),
            key("soon", "2026-06-01T00:00:00+0000"),
            key("later", "2026-09-01T00:00:00+0000"),
            format!("forever:{ED}"),
        ]
        .join("\n");
        let m = meta(json!({ "ssh-keys": lines }), json!({}));
        let d = desired(&m, at("2026-03-01T00:00:00Z"));
        assert_eq!(
            d.users.keys().collect::<Vec<_>>(),
            ["forever", "later", "soon"]
        );
        assert_eq!(d.next_expiry, Some(at("2026-06-01T00:00:00Z")));
        let d = desired(&m, at("2026-06-01T00:00:00Z"));
        assert_eq!(d.users.keys().collect::<Vec<_>>(), ["forever", "later"]);
    }

    #[test]
    fn enable_oslogin_applies_no_key() {
        let keys = json!({"ssh-keys": format!("alice:{ED}")});
        let mut on = keys.clone();
        on["enable-oslogin"] = json!("true");
        assert!(desired(&meta(on, json!({})), Utc::now()).users.is_empty());
        let d = desired(
            &meta(keys.clone(), json!({"enable-oslogin": "TRUE"})),
            Utc::now(),
        );
        assert!(d.oslogin && d.users.is_empty());
        // The instance's value overrides the project's.
        let mut off = keys;
        off["enable-oslogin"] = json!("false");
        let d = desired(&meta(off, json!({"enable-oslogin": "true"})), Utc::now());
        assert!(!d.oslogin);
        assert_eq!(d.users.len(), 1);
    }

    #[test]
    fn refused_lines_are_reported_and_the_rest_applies() {
        let m = meta(
            json!({"ssh-keys": format!("alice:{ED}\nBAD:{ED}")}),
            json!({}),
        );
        let d = desired(&m, Utc::now());
        assert_eq!(d.users.len(), 1);
        assert_eq!(d.refused.len(), 1);
        assert!(d.refused[0].starts_with("instance ssh-keys: "));
    }

    #[test]
    fn no_metadata_is_no_users() {
        assert_eq!(desired(&json!({}), Utc::now()), Desired::default());
    }
}
