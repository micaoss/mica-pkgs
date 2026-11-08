//! mica-gce-agent: SSH logins from Google Compute Engine metadata.
//!
//! gcloud compute ssh and the console's SSH button add `ssh-keys` lines to the
//! instance's or the project's metadata. The agent follows the metadata with a
//! long poll and, for each user those lines name, keeps a login account (Base's
//! mica-accounts, which also makes the home on DATA) and the user's key file,
//! which sshd reads as a second AuthorizedKeysFile for every user but root and
//! mica. It never writes root's or mica's keys, which are micad's, and never
//! the hostname.

pub mod accounts;
pub mod apply;
pub mod keys;
pub mod metadata;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;

use crate::accounts::Accounts;
use crate::apply::Paths;
use crate::metadata::Client;

/// The first wait after a failed request; it doubles up to `MAX_BACKOFF`.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Whether metadata users are admins (mica-accounts' admin group) now.
///
/// micad owns this switch, a visible and revocable grant, and writes it to
/// `grant`: `on` grants; anything else, an unreadable file or none at all is
/// no grant. It is read on every pass, at least once per long poll.
pub fn admin_granted(grant: &Path) -> bool {
    std::fs::read_to_string(grant).is_ok_and(|text| text.trim() == "on")
}

/// Apply the metadata `document` once, at the current time, and log what
/// changed. Returns when the earliest applied key expires.
fn apply_document(
    document: &Value,
    paths: &Paths,
    accounts: &dyn Accounts,
) -> Option<chrono::DateTime<Utc>> {
    let desired = keys::desired(document, Utc::now());
    for refused in &desired.refused {
        tracing::warn!("ssh-keys line refused: {refused}");
    }
    let admin = admin_granted(&paths.grant);
    match apply::apply(&desired.users, admin, paths, accounts) {
        Ok(report) => {
            for user in &report.accounts_set {
                tracing::info!(user, admin, "login account set");
            }
            for user in &report.accounts_removed {
                tracing::info!(user, "login account removed, home kept");
            }
            for user in &report.keys_written {
                tracing::info!(user, "keys written");
            }
            for user in &report.keys_removed {
                tracing::info!(user, "keys removed");
            }
            for skipped in &report.skipped {
                tracing::warn!("user not applied: {skipped}");
            }
        }
        Err(err) => tracing::error!("applying the metadata failed: {err:#}"),
    }
    desired.next_expiry
}

/// Follow the metadata until `shutdown` completes.
pub async fn run(
    client: Client,
    paths: Paths,
    accounts: Arc<dyn Accounts>,
    shutdown: impl Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    let mut document: Option<Value> = None;
    let mut etag: Option<String> = None;
    let mut expiry = None;
    let mut backoff = FIRST_BACKOFF;
    loop {
        let until_expiry = expiry
            .map(|at: chrono::DateTime<Utc>| (at - Utc::now()).to_std().unwrap_or(Duration::ZERO));
        tokio::select! {
            () = &mut shutdown => return,
            () = sleep_or_never(until_expiry) => {}
            answer = client.get(etag.as_deref()) => match answer {
                Ok(answer) => {
                    backoff = FIRST_BACKOFF;
                    etag = answer.etag;
                    document = Some(answer.document);
                }
                Err(err) => {
                    tracing::warn!("metadata: {err:#}; retrying in {backoff:?}");
                    tokio::select! {
                        () = &mut shutdown => return,
                        () = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    // A key may have expired while the server was away.
                    if !expiry.is_some_and(|at| at <= Utc::now()) {
                        continue;
                    }
                }
            },
        }
        if let Some(document) = &document {
            let doc = document.clone();
            let paths = paths.clone();
            let accounts = Arc::clone(&accounts);
            expiry = tokio::task::spawn_blocking(move || apply_document(&doc, &paths, &*accounts))
                .await
                .unwrap_or_else(|err| {
                    tracing::error!("applying the metadata panicked: {err}");
                    None
                });
        }
    }
}

async fn sleep_or_never(duration: Option<Duration>) {
    match duration {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_on_grants() {
        let dir = tempfile::tempdir().unwrap();
        let grant = dir.path().join("cloud-admins");
        assert!(!admin_granted(&grant), "a missing file is no grant");
        for (text, granted) in [
            ("on", true),
            ("on\n", true),
            ("off\n", false),
            ("ON\n", false),
            ("", false),
            ("yes\n", false),
            ("on on\n", false),
        ] {
            std::fs::write(&grant, text).unwrap();
            assert_eq!(admin_granted(&grant), granted, "{text:?}");
        }
        std::fs::remove_file(&grant).unwrap();
        std::fs::create_dir(&grant).unwrap();
        assert!(!admin_granted(&grant), "an unreadable switch is no grant");
    }
}
