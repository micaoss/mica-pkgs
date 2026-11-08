//! Make the key files, accounts and homes match what the metadata asks for.
//!
//! Each user's keys are one file, `<keys>/<user>`, which sshd reads as a
//! second AuthorizedKeysFile. The directory is in RAM and only the agent
//! writes it: a key that leaves the metadata, or expires, leaves the file, and
//! a user without keys has no file. Accounts and homes stay: mica-accounts
//! keeps the account, and a home on DATA is created once and never removed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::accounts::Accounts;

/// Where the agent writes.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The key files, one per user.
    pub keys: PathBuf,
    /// The parent of the homes.
    pub homes: PathBuf,
    /// The passwd file whose names, outside the login class, are never used.
    pub passwd: PathBuf,
}

/// What one pass did, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub accounts_added: Vec<String>,
    pub homes_created: Vec<String>,
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

/// Write `content` to `path` through a temporary file and a rename, so sshd
/// never reads half a file. The temporary name starts with a dot, which no
/// user name does. Unchanged content is not rewritten.
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

/// One pass over `users` (each user's authorized_keys lines).
pub fn apply(
    users: &BTreeMap<String, Vec<String>>,
    paths: &Paths,
    accounts: &dyn Accounts,
) -> Result<Report> {
    let mut report = Report::default();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&paths.keys)
        .with_context(|| format!("creating {}", paths.keys.display()))?;

    let mut logins = if users.is_empty() {
        BTreeMap::new()
    } else {
        accounts.list()?
    };
    let taken = if users.is_empty() {
        BTreeSet::new()
    } else {
        passwd_names(&paths.passwd)?
    };

    let mut written = BTreeSet::new();
    for (user, lines) in users {
        if taken.contains(user) && !logins.contains_key(user) {
            report
                .skipped
                .push(format!("{user}: an account outside the login class"));
            continue;
        }
        let account = match logins.get(user) {
            Some(account) => *account,
            None => match accounts.add(user) {
                Ok(account) => {
                    logins.insert(user.clone(), account);
                    report.accounts_added.push(user.clone());
                    account
                }
                Err(err) => {
                    report.skipped.push(format!("{user}: {err:#}"));
                    continue;
                }
            },
        };
        let home = paths.homes.join(user);
        if !home.exists() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&home)
                .with_context(|| format!("creating {}", home.display()))?;
            std::os::unix::fs::chown(&home, Some(account.uid), Some(account.gid))
                .with_context(|| format!("chown {}", home.display()))?;
            report.homes_created.push(user.clone());
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
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Account;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;

    /// mica-accounts in memory: ids from 20000, owned by the test's own uid
    /// so homes can be chowned without root.
    struct Stub {
        accounts: Mutex<BTreeMap<String, Account>>,
        refuse: Vec<String>,
        owner: Account,
    }

    impl Accounts for Stub {
        fn list(&self) -> Result<BTreeMap<String, Account>> {
            Ok(self.accounts.lock().unwrap().clone())
        }
        fn add(&self, name: &str) -> Result<Account> {
            anyhow::ensure!(!self.refuse.iter().any(|r| r == name), "refused {name}");
            let mut accounts = self.accounts.lock().unwrap();
            accounts.insert(name.to_string(), self.owner);
            Ok(self.owner)
        }
    }

    fn setup() -> (tempfile::TempDir, Paths, Stub) {
        let dir = tempfile::tempdir().unwrap();
        let meta = fs::metadata(dir.path()).unwrap();
        let paths = Paths {
            keys: dir.path().join("run/keys"),
            homes: dir.path().join("home"),
            passwd: dir.path().join("passwd"),
        };
        fs::create_dir(&paths.homes).unwrap();
        fs::write(
            &paths.passwd,
            "root:x:0:0::/root:/bin/sh\nmica:x:1000:1000::/home/mica:/bin/sh\n",
        )
        .unwrap();
        let stub = Stub {
            accounts: Mutex::new(BTreeMap::new()),
            refuse: vec![],
            owner: Account {
                uid: meta.uid(),
                gid: meta.gid(),
            },
        };
        (dir, paths, stub)
    }

    fn users(list: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        list.iter()
            .map(|(u, keys)| (u.to_string(), keys.iter().map(|k| k.to_string()).collect()))
            .collect()
    }

    #[test]
    fn keys_accounts_and_homes_appear() {
        let (_dir, paths, stub) = setup();
        let r = apply(
            &users(&[("alice", &["ssh-ed25519 AAAA a", "ssh-rsa BBBB"])]),
            &paths,
            &stub,
        )
        .unwrap();
        assert_eq!(r.accounts_added, ["alice"]);
        assert_eq!(r.homes_created, ["alice"]);
        assert_eq!(r.keys_written, ["alice"]);
        let file = paths.keys.join("alice");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "ssh-ed25519 AAAA a\nssh-rsa BBBB\n"
        );
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o644);
        assert_eq!(fs::metadata(&paths.keys).unwrap().mode() & 0o777, 0o755);
        assert_eq!(
            fs::metadata(paths.homes.join("alice")).unwrap().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn a_dotted_name_has_its_own_file_and_leftovers_go() {
        let (_dir, paths, stub) = setup();
        fs::create_dir_all(&paths.keys).unwrap();
        fs::write(paths.keys.join(".x.tmp"), "half").unwrap();
        let r = apply(
            &users(&[("a.b", &["ssh-ed25519 AAAA"]), ("a", &["ssh-rsa BBBB"])]),
            &paths,
            &stub,
        )
        .unwrap();
        assert!(r.keys_removed.is_empty());
        let mut names: Vec<_> = fs::read_dir(&paths.keys)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["a", "a.b"]);
        assert_eq!(
            fs::read_to_string(paths.keys.join("a.b")).unwrap(),
            "ssh-ed25519 AAAA\n"
        );
    }

    #[test]
    fn a_second_pass_changes_nothing() {
        let (_dir, paths, stub) = setup();
        let u = users(&[("alice", &["ssh-ed25519 AAAA"])]);
        apply(&u, &paths, &stub).unwrap();
        assert_eq!(apply(&u, &paths, &stub).unwrap(), Report::default());
    }

    #[test]
    fn a_user_without_keys_loses_the_file_but_keeps_home_and_account() {
        let (_dir, paths, stub) = setup();
        apply(
            &users(&[("alice", &["ssh-ed25519 AAAA"]), ("bob", &["ssh-rsa BBBB"])]),
            &paths,
            &stub,
        )
        .unwrap();
        fs::write(paths.homes.join("alice/notes"), "kept").unwrap();
        let r = apply(&users(&[("bob", &["ssh-rsa BBBB"])]), &paths, &stub).unwrap();
        assert_eq!(r.keys_removed, ["alice"]);
        assert!(!paths.keys.join("alice").exists());
        assert_eq!(
            fs::read_to_string(paths.homes.join("alice/notes")).unwrap(),
            "kept"
        );
        assert!(stub.list().unwrap().contains_key("alice"));
        let r = apply(&BTreeMap::new(), &paths, &stub).unwrap();
        assert_eq!(r.keys_removed, ["bob"]);
        assert_eq!(fs::read_dir(&paths.keys).unwrap().count(), 0);
    }

    #[test]
    fn names_outside_the_login_class_are_never_used() {
        let (_dir, paths, stub) = setup();
        let r = apply(
            &users(&[
                ("root", &["ssh-ed25519 AAAA"]),
                ("mica", &["ssh-ed25519 AAAA"]),
            ]),
            &paths,
            &stub,
        )
        .unwrap();
        assert_eq!(r.skipped.len(), 2);
        assert!(r.accounts_added.is_empty());
        assert_eq!(fs::read_dir(&paths.keys).unwrap().count(), 0);
        assert!(!paths.homes.join("root").exists());
    }

    #[test]
    fn a_login_account_already_in_passwd_is_used() {
        let (_dir, paths, stub) = setup();
        fs::write(&paths.passwd, "alice:x:20000:20000::/home/alice:/bin/sh\n").unwrap();
        stub.accounts
            .lock()
            .unwrap()
            .insert("alice".into(), stub.owner);
        let r = apply(&users(&[("alice", &["ssh-ed25519 AAAA"])]), &paths, &stub).unwrap();
        assert!(r.accounts_added.is_empty());
        assert_eq!(r.keys_written, ["alice"]);
    }

    #[test]
    fn a_refused_account_skips_only_that_user() {
        let (_dir, paths, mut stub) = setup();
        stub.refuse = vec!["bob".into()];
        let r = apply(
            &users(&[("alice", &["ssh-ed25519 AAAA"]), ("bob", &["ssh-rsa BBBB"])]),
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
    fn an_existing_home_is_left_alone() {
        let (_dir, paths, stub) = setup();
        fs::create_dir(paths.homes.join("alice")).unwrap();
        fs::set_permissions(paths.homes.join("alice"), fs::Permissions::from_mode(0o750)).unwrap();
        let r = apply(&users(&[("alice", &["ssh-ed25519 AAAA"])]), &paths, &stub).unwrap();
        assert!(r.homes_created.is_empty());
        assert_eq!(
            fs::metadata(paths.homes.join("alice")).unwrap().mode() & 0o777,
            0o750
        );
    }
}
