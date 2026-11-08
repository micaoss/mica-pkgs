//! The login accounts of metadata users, kept by Base's mica-accounts.
//!
//! The root is read-only and /etc/passwd is a build of mica-accounts, so the
//! agent never writes it. It uses two commands of mica-accounts' login class:
//!
//!   mica-accounts login-list
//!       one `<name> TAB <uid> TAB <gid>` line per login account
//!   mica-accounts login-add <name> <group>
//!       create the account, or keep it, with <group> as a supplementary
//!       group; print its `<name> TAB <uid> TAB <gid>` line
//!
//! mica-accounts picks the uid and keeps the account on STATE, so it and the
//! home it owns survive a reboot.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// A login account's ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
}

/// Where the login accounts come from.
pub trait Accounts: Send + Sync {
    /// Every login account, by name.
    fn list(&self) -> Result<BTreeMap<String, Account>>;
    /// Create `name`, or keep it, and return its ids.
    fn add(&self, name: &str) -> Result<Account>;
}

/// mica-accounts, run as a command.
pub struct Tool {
    pub path: PathBuf,
    /// The supplementary group every login account is in.
    pub group: String,
}

fn parse(line: &str) -> Result<(String, Account)> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [name, uid, gid] = fields[..] else {
        bail!("`{line}` is not <name> TAB <uid> TAB <gid>");
    };
    let id = |text: &str| {
        text.parse::<u32>()
            .with_context(|| format!("`{text}` in `{line}` is not an id"))
    };
    Ok((
        name.to_string(),
        Account {
            uid: id(uid)?,
            gid: id(gid)?,
        },
    ))
}

impl Tool {
    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.path)
            .args(args)
            .output()
            .with_context(|| format!("running {}", self.path.display()))?;
        if !output.status.success() {
            bail!(
                "{} {} failed ({}): {}",
                self.path.display(),
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout).context("mica-accounts printed no UTF-8")
    }
}

impl Accounts for Tool {
    fn list(&self) -> Result<BTreeMap<String, Account>> {
        self.run(&["login-list"])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse)
            .collect()
    }

    fn add(&self, name: &str) -> Result<Account> {
        let out = self.run(&["login-add", name, &self.group])?;
        let (got, account) = parse(out.trim_end_matches('\n'))?;
        if got != name {
            bail!("login-add {name} answered for `{got}`");
        }
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tool(dir: &tempfile::TempDir, script: &str) -> Tool {
        let path = dir.path().join("mica-accounts");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Tool {
            path,
            group: "mica-gce-admins".into(),
        }
    }

    #[test]
    fn list_and_add_read_the_tool_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let t = tool(
            &dir,
            &format!(
                r#"echo "$*" >> {log}
case "$1" in
login-list) printf 'alice\t20000\t20000\nbob\t20001\t20001\n' ;;
login-add) printf '%s\t20002\t20002\n' "$2" ;;
esac"#,
                log = log.display()
            ),
        );
        let list = t.list().unwrap();
        assert_eq!(
            list["bob"],
            Account {
                uid: 20001,
                gid: 20001
            }
        );
        assert_eq!(
            t.add("carol").unwrap(),
            Account {
                uid: 20002,
                gid: 20002
            }
        );
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "login-list\nlogin-add carol mica-gce-admins\n"
        );
    }

    #[test]
    fn a_failing_or_garbled_tool_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let failing = tool(&dir, "echo 'mica-accounts: refused' >&2; exit 1");
        let err = failing.add("alice").unwrap_err().to_string();
        assert!(err.contains("mica-accounts: refused"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        assert!(tool(&dir, "echo 'alice 1 2'").list().is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(tool(&dir, "printf 'bob\\t1\\t2\\n'").add("alice").is_err());
    }
}
