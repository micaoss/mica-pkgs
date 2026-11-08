//! `mica-gce-agent` — SSH logins from Google Compute Engine metadata.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use mica_gce_agent::accounts::Tool;
use mica_gce_agent::apply::Paths;
use mica_gce_agent::metadata::Client;

/// Follows GCE metadata ssh-keys: login accounts and key files.
#[derive(Debug, Parser)]
#[command(name = "mica-gce-agent", version)]
struct Args {
    /// The metadata server; metadata.google.internal's address, so no name
    /// has to resolve.
    #[arg(long, default_value = "169.254.169.254:80")]
    metadata: SocketAddr,

    /// The key files sshd reads as `/run/mica/authorized-keys/%u`.
    #[arg(long, default_value = "/run/mica/authorized-keys")]
    keys: PathBuf,

    /// micad's grant: `on` puts metadata users in the admin group.
    #[arg(long, default_value = "/run/mica/cloud-admins")]
    cloud_admins: PathBuf,

    /// Names here that are not login accounts are never used.
    #[arg(long, default_value = "/etc/passwd")]
    passwd: PathBuf,

    /// Base's account tool and its login class.
    #[arg(long, default_value = "/usr/lib/mica/mica-accounts")]
    accounts_tool: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .init();
    let args = Args::parse();
    tracing::info!(metadata = %args.metadata, keys = %args.keys.display(), "starting");
    mica_gce_agent::run(
        Client::new(args.metadata),
        Paths {
            keys: args.keys,
            passwd: args.passwd,
            grant: args.cloud_admins,
        },
        Arc::new(Tool {
            path: args.accounts_tool,
        }),
        std::future::pending(),
    )
    .await;
}
