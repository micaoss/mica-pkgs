//! Phone credentials (protocol section 7.2), kept in one JSON file that only
//! the service reads.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

/// The longest phone name accepted, in characters.
pub const MAX_NAME: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Phone {
    pub phone: String,
    pub name: String,
    pub created: u64,
    secret: String,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    phones: Vec<Phone>,
}

pub struct Phones {
    path: PathBuf,
    phones: Vec<Phone>,
}

impl Phones {
    /// Read the store; an absent file is no phones.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let phones = match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice::<File>(&bytes)
                    .map_err(|err| anyhow::anyhow!("{}: {err}", path.display()))?
                    .phones
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(anyhow::anyhow!("{}: {err}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            phones,
        })
    }

    /// The 32-byte credential of a phone.
    pub fn secret(&self, phone: &str) -> Option<Vec<u8>> {
        let entry = self.phones.iter().find(|p| p.phone == phone)?;
        STANDARD.decode(&entry.secret).ok()
    }

    pub fn list(&self) -> &[Phone] {
        &self.phones
    }

    /// Issue a credential; returns the phone id and the secret.
    pub fn enroll(&mut self, name: &str, now: u64) -> anyhow::Result<(String, [u8; 32])> {
        let mut id = [0u8; 16];
        let mut secret = [0u8; 32];
        getrandom::fill(&mut id).map_err(|err| anyhow::anyhow!("random: {err}"))?;
        getrandom::fill(&mut secret).map_err(|err| anyhow::anyhow!("random: {err}"))?;
        let phone: String = id.iter().map(|b| format!("{b:02x}")).collect();
        let mut next = self.phones.clone();
        next.push(Phone {
            phone: phone.clone(),
            name: name.to_string(),
            created: now,
            secret: STANDARD.encode(secret),
        });
        self.save(next)?;
        Ok((phone, secret))
    }

    /// Revoke a credential; false when there was none.
    pub fn remove(&mut self, phone: &str) -> anyhow::Result<bool> {
        let next: Vec<Phone> = self
            .phones
            .iter()
            .filter(|p| p.phone != phone)
            .cloned()
            .collect();
        if next.len() == self.phones.len() {
            return Ok(false);
        }
        self.save(next)?;
        Ok(true)
    }

    /// Replace the file atomically, mode 0600, and only then the memory copy.
    fn save(&mut self, phones: Vec<Phone>) -> anyhow::Result<()> {
        let file = File { phones };
        let tmp = self.path.with_extension("tmp");
        let write = || -> std::io::Result<()> {
            let _ = std::fs::remove_file(&tmp);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec(&file)?)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        };
        write().map_err(|err| anyhow::anyhow!("{}: {err}", self.path.display()))?;
        self.phones = file.phones;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn enroll_persists_and_remove_revokes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phones.json");
        let mut store = Phones::load(&path).unwrap();
        assert!(store.list().is_empty());
        let (id, secret) = store.enroll("Pixel 9", 1_800_000_000).unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(store.secret(&id).unwrap(), secret);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let reloaded = Phones::load(&path).unwrap();
        assert_eq!(reloaded.secret(&id).unwrap(), secret);
        assert_eq!(reloaded.list()[0].name, "Pixel 9");
        assert_eq!(reloaded.list()[0].created, 1_800_000_000);

        let mut store = reloaded;
        let (other, _) = store.enroll("iPhone", 1).unwrap();
        assert_ne!(other, id);
        assert!(store.remove(&id).unwrap());
        assert!(!store.remove(&id).unwrap());
        assert!(store.secret(&id).is_none());
        assert!(Phones::load(&path).unwrap().secret(&other).is_some());
    }

    #[test]
    fn a_corrupt_store_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phones.json");
        std::fs::write(&path, b"{").unwrap();
        assert!(Phones::load(&path).is_err());
    }
}
