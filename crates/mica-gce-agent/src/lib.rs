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
/// micad owns this switch, a visible and revocable grant that is on by default
/// on Compute Engine products; until its read path exists, the grant is on.
fn admin_granted() -> bool {
    true
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
    let admin = admin_granted();
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
