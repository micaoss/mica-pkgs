//! Make the login accounts and key files match what the metadata asks for.
//!
//! Each user's keys are one file, `<keys>/<user>`, which sshd reads as a
//! second AuthorizedKeysFile for every user but root and mica. Only the agent
//! writes there: a key that leaves the metadata, or expires, leaves the file,
//! and a user without keys has no file and no login account. Homes are
//! mica-accounts' and stay.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::accounts::Accounts;

/// Names never given keys, whatever the metadata says: micad's.
const RESERVED: &[&str] = &["root", "mica"];

/// Where the agent reads and writes.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The key files, one per user.
    pub keys: PathBuf,
    /// The passwd file whose names, outside the login class, are never used.
    pub passwd: PathBuf,
    /// micad's grant switch for the admin group (`crate::admin_granted`).
    pub grant: PathBuf,
}

/// What one pass did, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Accounts created, or made an admin or no longer one.
    pub accounts_set: Vec<String>,
    pub accounts_removed: Vec<String>,
    pub keys_written: Vec<String>,
    pub keys_removed: Vec<String>,
    /// Users not applied, with the reason.
    pub skipped: Vec<String>,
}

fn passwd_names(path: &Path) -> Result<BTreeSet<String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|line| line.split(':').next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// Write `content` to `<dir>/<user>` through a temporary file and a rename,
/// so sshd never reads half a file. The temporary name starts with a dot,
/// which no user name does. Unchanged content is not rewritten.
fn write_keys(dir: &Path, user: &str, content: &str) -> Result<bool> {
    let path = dir.join(user);
    if fs::read_to_string(&path).is_ok_and(|old| old == content) {
        return Ok(false);
    }
    let tmp = dir.join(format!(".{user}.tmp"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(content.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    drop(file);
    fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(true)
}

/// One pass over `users` (each user's authorized_keys lines); `admin` is
/// whether metadata users are admins now.
pub fn apply(
    users: &BTreeMap<String, Vec<String>>,
    admin: bool,
    paths: &Paths,
    accounts: &dyn Accounts,
) -> Result<Report> {
    let mut report = Report::default();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&paths.keys)
        .with_context(|| format!("creating {}", paths.keys.display()))?;

    let logins = accounts.list()?;
    let taken = passwd_names(&paths.passwd)?;

    let mut written = BTreeSet::new();
    for (user, lines) in users {
        if RESERVED.contains(&user.as_str()) || (taken.contains(user) && !logins.contains_key(user))
        {
            report
                .skipped
                .push(format!("{user}: an account outside the login class"));
            continue;
        }
        // add-login sets admin only when it creates the account; after
        // that, set-login-admin follows micad's switch.
        let set = match logins.get(user) {
            None => accounts.add(user, admin).map(|_| true),
            Some(login) if login.admin != admin => accounts.set_admin(user, admin).map(|()| true),
            Some(_) => Ok(false),
        };
        match set {
            Ok(true) => report.accounts_set.push(user.clone()),
            Ok(false) => {}
            Err(err) => {
                report.skipped.push(format!("{user}: {err:#}"));
                continue;
            }
        }
        let content = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        if write_keys(&paths.keys, user, &content)? {
            report.keys_written.push(user.clone());
        }
        written.insert(user.clone());
    }

    for entry in fs::read_dir(&paths.keys)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !written.contains(&name) {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
            if !name.starts_with('.') {
                report.keys_removed.push(name);
            }
        }
    }

    for user in logins.keys().filter(|user| !users.contains_key(*user)) {
        match accounts.remove(user) {
            Ok(()) => report.accounts_removed.push(user.clone()),
            Err(err) => report.skipped.push(format!("{user}: {err:#}")),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Login;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;

    /// mica-accounts in memory.
    #[derive(Default)]
    struct Stub {
        /// name -> admin
        accounts: Mutex<BTreeMap<String, bool>>,
        refuse: Vec<String>,
        calls: Mutex<Vec<String>>,
    }

    impl Accounts for Stub {
        fn list(&self) -> Result<BTreeMap<String, Login>> {
            Ok(self
                .accounts
                .lock()
                .unwrap()
                .iter()
                .zip(20000..)
                .map(|((name, admin), uid)| (name.clone(), Login { uid, admin: *admin }))
                .collect())
        }
        /// As mica-accounts: admin counts only when the account is made.
        fn add(&self, name: &str, admin: bool) -> Result<u32> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("add {name} {admin}"));
            anyhow::ensure!(!self.refuse.iter().any(|r| r == name), "refused {name}");
            self.accounts
                .lock()
                .unwrap()
                .entry(name.to_string())
                .or_insert(admin);
            Ok(20000)
        }
        fn set_admin(&self, name: &str, admin: bool) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set-admin {name} {admin}"));
            let mut accounts = self.accounts.lock().unwrap();
            let entry = accounts
                .get_mut(name)
                .ok_or_else(|| anyhow::anyhow!("no {name}"))?;
            *entry = admin;
            Ok(())
        }
        fn remove(&self, name: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("remove {name}"));
            self.accounts.lock().unwrap().remove(name);
            Ok(())
        }
    }

    impl Stub {
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    fn setup() -> (tempfile::TempDir, Paths, Stub) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            keys: dir.path().join("run/authorized-keys"),
            passwd: dir.path().join("passwd"),
            grant: dir.path().join("cloud-admins"),
        };
        fs::write(
            &paths.passwd,
            "root:x:0:0::/root:/bin/sh\nmica:x:1000:1000::/home/mica:/bin/sh\nsshd:x:22:22::/:/bin/false\n",
        )
        .unwrap();
        (dir, paths, Stub::default())
    }

    fn users(list: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        list.iter()
            .map(|(u, keys)| (u.to_string(), keys.iter().map(|k| k.to_string()).collect()))
            .collect()
    }

    #[test]
    fn accounts_and_keys_appear() {
        let (_dir, paths, stub) = setup();
        let r = apply(
            &users(&[("alice", &["ssh-ed25519 AAAA a", "ssh-rsa BBBB"])]),
            true,
            &paths,
            &stub,
        )
        .unwrap();
        assert_eq!(r.accounts_set, ["alice"]);
        assert_eq!(r.keys_written, ["alice"]);
        assert_eq!(stub.calls(), ["add alice true"]);
        let file = paths.keys.join("alice");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "ssh-ed25519 AAAA a\nssh-rsa BBBB\n"
        );
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o644);
        assert_eq!(fs::metadata(&paths.keys).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn a_second_pass_changes_nothing() {
        let (_dir, paths, stub) = setup();
        let u = users(&[("alice", &["ssh-ed25519 AAAA"])]);
        apply(&u, true, &paths, &stub).unwrap();
        stub.calls();
        assert_eq!(apply(&u, true, &paths, &stub).unwrap(), Report::default());
        assert!(stub.calls().is_empty());
    }

    #[test]
    fn the_admin_grant_follows_the_switch() {
        let (_dir, paths, stub) = setup();
        let u = users(&[("alice", &["ssh-ed25519 AAAA"])]);
        apply(&u, true, &paths, &stub).unwrap();
        stub.calls();
        let r = apply(&u, false, &paths, &stub).unwrap();
        assert_eq!(r.accounts_set, ["alice"]);
        assert!(r.keys_written.is_empty());
        assert_eq!(stub.calls(), ["set-admin alice false"]);
        assert!(!stub.list().unwrap()["alice"].admin);
        assert_eq!(apply(&u, false, &paths, &stub).unwrap(), Report::default());
        apply(&u, true, &paths, &stub).unwrap();
        assert_eq!(stub.calls(), ["set-admin alice true"]);
        assert!(stub.list().unwrap()["alice"].admin);
    }

    #[test]
    fn a_user_without_keys_loses_file_and_account() {
        let (_dir, paths, stub) = setup();
        let both = users(&[("alice", &["ssh-ed25519 AAAA"]), ("bob", &["ssh-rsa BBBB"])]);
        apply(&both, true, &paths, &stub).unwrap();
        stub.calls();
        let r = apply(&users(&[("bob", &["ssh-rsa BBBB"])]), true, &paths, &stub).unwrap();
        assert_eq!(r.keys_removed, ["alice"]);
        assert_eq!(r.accounts_removed, ["alice"]);
        assert_eq!(stub.calls(), ["remove alice"]);
        assert!(!paths.keys.join("alice").exists());
        let r = apply(&BTreeMap::new(), true, &paths, &stub).unwrap();
        assert_eq!(r.keys_removed, ["bob"]);
        assert_eq!(r.accounts_removed, ["bob"]);
        assert_eq!(fs::read_dir(&paths.keys).unwrap().count(), 0);
    }

    #[test]
    fn root_mica_and_system_names_are_never_used() {
        let (_dir, paths, stub) = setup();
        fs::write(&paths.passwd, "sshd:x:22:22::/:/bin/false\n").unwrap();
        let r = apply(
            &users(&[
                ("root", &["ssh-ed25519 AAAA"]),
                ("mica", &["ssh-ed25519 AAAA"]),
                ("sshd", &["ssh-ed25519 AAAA"]),
            ]),
            true,
            &paths,
            &stub,
        )
        .unwrap();
        assert_eq!(r.skipped.len(), 3);
        assert!(stub.calls().is_empty());
        assert_eq!(fs::read_dir(&paths.keys).unwrap().count(), 0);
    }

    #[test]
    fn a_login_account_already_in_passwd_is_used() {
        let (_dir, paths, stub) = setup();
        fs::write(&paths.passwd, "alice:x:20000:20000::/home/alice:/bin/sh\n").unwrap();
        stub.accounts.lock().unwrap().insert("alice".into(), true);
        let r = apply(
            &users(&[("alice", &["ssh-ed25519 AAAA"])]),
            true,
            &paths,
            &stub,
        )
        .unwrap();
        assert!(r.accounts_set.is_empty());
        assert_eq!(r.keys_written, ["alice"]);
    }

    #[test]
    fn a_refused_account_skips_only_that_user() {
        let (_dir, paths, mut stub) = setup();
        stub.refuse = vec!["bob".into()];
        let r = apply(
            &users(&[("alice", &["ssh-ed25519 AAAA"]), ("bob", &["ssh-rsa BBBB"])]),
            true,
            &paths,
            &stub,
        )
        .unwrap();
        assert_eq!(r.keys_written, ["alice"]);
        assert_eq!(r.skipped.len(), 1);
        assert!(
            r.skipped[0].starts_with("bob: refused bob"),
            "{:?}",
            r.skipped
        );
        assert!(!paths.keys.join("bob").exists());
    }

    #[test]
    fn leftover_temporary_files_go() {
        let (_dir, paths, stub) = setup();
        fs::create_dir_all(&paths.keys).unwrap();
        fs::write(paths.keys.join(".x.tmp"), "half").unwrap();
        let r = apply(
            &users(&[("a-b", &["ssh-ed25519 AAAA"])]),
            true,
            &paths,
            &stub,
        )
        .unwrap();
        assert!(r.keys_removed.is_empty());
        let names: Vec<_> = fs::read_dir(&paths.keys)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a-b"]);
    }
}
