//! The login accounts of metadata users, kept by Base's mica-accounts.
//!
//! The root is read-only and /etc/passwd is a build of mica-accounts, so the
//! agent never writes it. It uses mica-accounts' login class:
//!
//!   mica-accounts list-login
//!       one `<name> TAB <uid>` line per login account
//!   mica-accounts add-login <name> [--admin]
//!       create the account, or keep it, in the admin group or out of it;
//!       print its uid
//!   mica-accounts remove-login <name>
//!       drop the account, keeping its home
//!
//! mica-accounts picks the uid in 20000..29999, makes the home /home/<name>
//! on DATA and keeps the account on STATE. Who is an admin now is read from
//! the group file's admin group.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Where the login accounts come from.
pub trait Accounts: Send + Sync {
    /// Every login account, with its uid.
    fn list(&self) -> Result<BTreeMap<String, u32>>;
    /// The members of the admin group.
    fn admins(&self) -> Result<BTreeSet<String>>;
    /// Create `name`, or keep it, as an admin or not; its uid.
    fn add(&self, name: &str, admin: bool) -> Result<u32>;
    /// Drop `name`; its home stays.
    fn remove(&self, name: &str) -> Result<()>;
}

/// mica-accounts, run as a command.
pub struct Tool {
    pub path: PathBuf,
    /// The group file the admin group is read from.
    pub group_file: PathBuf,
    /// The group of the admins.
    pub admin_group: String,
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

fn uid(text: &str) -> Result<u32> {
    text.trim()
        .parse()
        .with_context(|| format!("`{}` is not a uid", text.trim()))
}

impl Accounts for Tool {
    fn list(&self) -> Result<BTreeMap<String, u32>> {
        self.run(&["list-login"])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let Some((name, id)) = line.split_once('\t') else {
                    bail!("`{line}` is not <name> TAB <uid>");
                };
                Ok((name.to_string(), uid(id)?))
            })
            .collect()
    }

    fn admins(&self) -> Result<BTreeSet<String>> {
        let text = std::fs::read_to_string(&self.group_file)
            .with_context(|| format!("reading {}", self.group_file.display()))?;
        Ok(text
            .lines()
            .find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                (fields.len() == 4 && fields[0] == self.admin_group).then(|| fields[3])
            })
            .unwrap_or_default()
            .split(',')
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn add(&self, name: &str, admin: bool) -> Result<u32> {
        let mut args = vec!["add-login", name];
        if admin {
            args.push("--admin");
        }
        uid(&self.run(&args)?)
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.run(&["remove-login", name]).map(|_| ())
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
        let group_file = dir.path().join("group");
        std::fs::write(
            &group_file,
            "root:x:0:\nmica-admins:x:982:alice,bob\nmica-admins-old:x:983:carol\n",
        )
        .unwrap();
        Tool {
            path,
            group_file,
            admin_group: "mica-admins".into(),
        }
    }

    #[test]
    fn the_login_commands_are_called_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let t = tool(
            &dir,
            &format!(
                r#"echo "$*" >> {log}
case "$1" in
list-login) printf 'alice\t20000\nbob\t20001\n' ;;
add-login) echo 20002 ;;
esac"#,
                log = log.display()
            ),
        );
        assert_eq!(t.list().unwrap()["bob"], 20001);
        assert_eq!(t.add("carol", true).unwrap(), 20002);
        assert_eq!(t.add("dave", false).unwrap(), 20002);
        t.remove("alice").unwrap();
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "list-login\nadd-login carol --admin\nadd-login dave\nremove-login alice\n"
        );
    }

    #[test]
    fn admins_are_the_admin_group_members() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(&dir, "true");
        assert_eq!(
            t.admins().unwrap().into_iter().collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        std::fs::write(&t.group_file, "mica-admins:x:982:\n").unwrap();
        assert!(t.admins().unwrap().is_empty());
        std::fs::write(&t.group_file, "root:x:0:\n").unwrap();
        assert!(t.admins().unwrap().is_empty());
    }

    #[test]
    fn a_failing_or_garbled_tool_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let failing = tool(&dir, "echo 'mica-accounts: refused' >&2; exit 1");
        let err = failing.add("alice", false).unwrap_err().to_string();
        assert!(err.contains("mica-accounts: refused"), "{err}");
        let dir = tempfile::tempdir().unwrap();
        assert!(tool(&dir, "echo 'alice 1'").list().is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(tool(&dir, "echo none").add("alice", false).is_err());
    }
}
