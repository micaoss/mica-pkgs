//! `mica-bled` — mica-apid over Bluetooth LE for the phone app.
//!
//! The protocol is in the library ([`mica_bled`]); this is the command line,
//! the logger and the D-Bus connection.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use mica_bled::apid::HttpApid;
use mica_bled::phones::Phones;
use mica_bled::session::Device;
use tokio::signal::unix::{SignalKind, signal};

/// The BLE management service: authenticates phones and forwards their
/// network requests to mica-apid.
#[derive(Debug, Parser)]
#[command(name = "mica-bled", version)]
struct Args {
    /// micad's device id: SPAKE2's identity B.
    #[arg(long)]
    device_id: String,

    /// mica-apid's local socket.
    #[arg(long, default_value = "/run/mica/apid.sock")]
    apid: PathBuf,

    /// Where the phone credentials are kept.
    #[arg(long, default_value = "/var/lib/mica-bled")]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    anyhow::ensure!(!args.device_id.is_empty(), "--device-id is empty");
    let phones = Phones::load(&args.state_dir.join("phones.json"))?;
    let apid = Arc::new(HttpApid { socket: args.apid });
    let device = Arc::new(Device::new(args.device_id, apid, phones));
    let conn = zbus::Connection::system()
        .await
        .context("connecting to the system bus")?;
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        result = mica_bled::gatt::run(conn, device) => result,
        _ = term.recv() => Ok(()),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
