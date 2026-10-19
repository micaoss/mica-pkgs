//! `mica-mqttd` — the MQTT bridge over explicitly enrolled application trees.
//!
//! Everything the daemon does is in the library ([`mica_mqttd`]); this is the
//! command line, the logger and the call into [`mica_mqttd::runtime::run`].

#![forbid(unsafe_code)]

use clap::Parser;
use mica_mqttd::config::{Mode, Timings};
use mica_mqttd::runtime::{self, Settings};

/// The MQTT data-publishing bridge for explicitly enrolled `com.mica.*` applications.
#[derive(Debug, Parser)]
#[command(name = "mica-mqttd", version)]
struct Args {
    /// Stable device identity used as the MQTT topic address.
    #[arg(long)]
    device_id: String,

    /// Package-owned directory whose file names are exact application D-Bus names.
    #[arg(long, default_value = "/usr/lib/mica/mqtt-applications.d")]
    applications_dir: std::path::PathBuf,

    /// Broker host to connect to.
    #[arg(long, default_value = "localhost")]
    broker_host: String,

    /// Broker port.
    #[arg(long, default_value_t = 1883)]
    broker_port: u16,

    /// Private JSON file containing the broker username and password.
    #[arg(long, default_value = "/var/lib/mica/mqttd-credentials.json")]
    credentials_file: std::path::PathBuf,

    /// MQTT client id. Must be unique on the broker.
    #[arg(long, default_value = "mica-mqttd")]
    client_id: String,

    /// Whether write requests reach the bus. Defaults to read-only: a bridge
    /// nobody configured cannot be a control path.
    #[arg(long, value_enum, default_value_t = Mode::ReadOnly)]
    mode: Mode,

    /// Use the session bus instead of the system bus.
    #[arg(long)]
    session_bus: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let args = Args::parse();
    tracing::info!(
        broker = format!("{}:{}", args.broker_host, args.broker_port),
        mode = ?args.mode,
        "starting"
    );
    runtime::run(Settings {
        device_id: args.device_id,
        applications_dir: args.applications_dir,
        broker_host: args.broker_host,
        broker_port: args.broker_port,
        client_id: args.client_id,
        credentials_file: args.credentials_file,
        mode: args.mode,
        session_bus: args.session_bus,
        timings: Timings::default(),
    })
    .await
}
