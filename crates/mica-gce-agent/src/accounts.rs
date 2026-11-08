//! The login accounts of metadata users, kept by Base's mica-accounts.
//!
//! The root is read-only and /etc/passwd is a build of mica-accounts, so the
//! agent never writes it. It uses mica-accounts' login class:
//!
//!   mica-accounts list-login
//!       one `<name> TAB <uid> TAB yes|no` line per login account; the third
//!       field is whether it is an admin
//!   mica-accounts add-login <name> [--admin]
//!       create the account, or keep it, an admin with --admin and no longer
//!       one without, and print its uid
//!   mica-accounts remove-login <name>
//!       drop the account, keeping its home
//!
//! mica-accounts picks the uid in 20000..29999, makes the home /home/<name>
//! on DATA and keeps the account on STATE.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// One login account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Login {
    pub uid: u32,
    pub admin: bool,
}

/// Where the login accounts come from.
pub trait Accounts: Send + Sync {
    /// Every login account.
    fn list(&self) -> Result<BTreeMap<String, Login>>;
    /// Create or keep `name`, an admin or no longer one; its uid.
    fn add(&self, name: &str, admin: bool) -> Result<u32>;
    /// Drop `name`; its home stays.
    fn remove(&self, name: &str) -> Result<()>;
}

/// mica-accounts, run as a command.
pub struct Tool {
    pub path: PathBuf,
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

fn parse(line: &str) -> Result<(String, Login)> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [name, id, admin] = fields[..] else {
        bail!("`{line}` is not <name> TAB <uid> TAB yes|no");
    };
    let admin = match admin {
        "yes" => true,
        "no" => false,
        _ => bail!("`{line}`: the admin field is not yes or no"),
    };
    Ok((
        name.to_string(),
        Login {
            uid: uid(id)?,
            admin,
        },
    ))
}

impl Accounts for Tool {
    fn list(&self) -> Result<BTreeMap<String, Login>> {
        self.run(&["list-login"])?
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse)
            .collect()
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
        Tool { path }
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
list-login) printf 'alice\t20000\tyes\nbob\t20001\tno\n' ;;
add-login) echo 20002 ;;
esac"#,
                log = log.display()
            ),
        );
        let list = t.list().unwrap();
        assert_eq!(
            list["alice"],
            Login {
                uid: 20000,
                admin: true
            }
        );
        assert_eq!(
            list["bob"],
            Login {
                uid: 20001,
                admin: false
            }
        );
        assert_eq!(t.add("carol", true).unwrap(), 20002);
        assert_eq!(t.add("dave", false).unwrap(), 20002);
        t.remove("alice").unwrap();
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "list-login\nadd-login carol --admin\nadd-login dave\nremove-login alice\n"
        );
    }

    #[test]
    fn a_failing_or_garbled_tool_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let failing = tool(&dir, "echo 'mica-accounts: refused' >&2; exit 1");
        let err = failing.add("alice", false).unwrap_err().to_string();
        assert!(err.contains("mica-accounts: refused"), "{err}");
        assert!(failing.add("alice", true).is_err());
        for listing in [
            "echo 'alice 1 yes'",
            "printf 'alice\\t20000\\n'",
            "printf 'alice\\t20000\\tmaybe\\n'",
            "printf 'alice\\tx\\tno\\n'",
        ] {
            let dir = tempfile::tempdir().unwrap();
            assert!(tool(&dir, listing).list().is_err(), "{listing}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(tool(&dir, "echo none").add("alice", false).is_err());
    }
}
