//! Protocol tests for the MQTT bridge.
//!
//! These drive the production path — the real [`Bridge`], the real payload
//! encoder and masker, the real [`apply`] — against an in-memory
//! [`Transport`] and [`ItemSource`], per the decision recorded in the crate
//! docs. No broker, no D-Bus daemon, no filesystem: **none of these tests can
//! skip**, which is deliberate, because a test that skips reports green while
//! asserting nothing.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use mica_mqttd::bridge::{Bridge, Effects, Publication};
use mica_mqttd::config::{Mode, Timings};
use mica_mqttd::enrollment::Enrollment;
use mica_mqttd::item::Item;
use mica_mqttd::runtime::apply;
use mica_mqttd::source::{ItemSource, WriteOutcome};
use mica_mqttd::topic;
use mica_mqttd::transport::Transport;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

mod publish;
mod reconnect;
mod topics;

/// The device identity supplied as root-rendered runtime configuration.
const DEVICE: &str = "abc123";
/// One exact service enrolled by its application package.
const APPLICATION_SERVICE: &str = "com.mica.sensor.abc123";
/// The class [`APPLICATION_SERVICE`] must publish under.
const APPLICATION_CLASS: &str = "sensor";
const CLASS: &str = APPLICATION_CLASS;

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// Everything an item notification for `path` is addressed by.
fn notify(path: &str) -> String {
    format!("N/{DEVICE}/{CLASS}/0{path}")
}

/// A representative application tree. Device identity is deliberately absent:
/// it is process runtime configuration, outside every publishable item tree.
fn tree() -> BTreeMap<String, Item> {
    BTreeMap::from([
        ("/DeviceInstance".to_string(), Item::new(json!(0))),
        ("/Temperature".to_string(), Item::new(json!(21))),
        ("/SampleCount".to_string(), Item::new(json!(42))),
        ("/Enabled".to_string(), Item::writable(json!(true))),
        ("/Calibration/offset".to_string(), Item::writable(json!(0))),
    ])
}

fn application() -> topic::Application {
    application_named(APPLICATION_SERVICE)
}

fn application_named(bus_name: &str) -> topic::Application {
    Enrollment::from_names([bus_name])
        .expect("fixture is a valid enrollment")
        .application(bus_name)
        .expect("fixture name is enrolled")
}

/// A [`Transport`] that records instead of connecting.
#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<Publication>>,
    filters: Mutex<Vec<String>>,
}

#[async_trait]
impl Transport for Recorder {
    async fn publish(&self, publication: &Publication) -> anyhow::Result<()> {
        self.published.lock().unwrap().push(publication.clone());
        Ok(())
    }

    async fn subscribe(&self, filter: &str) -> anyhow::Result<()> {
        self.filters.lock().unwrap().push(filter.to_string());
        Ok(())
    }

    async fn unsubscribe(&self, filter: &str) -> anyhow::Result<()> {
        self.filters.lock().unwrap().retain(|held| held != filter);
        Ok(())
    }
}

impl Recorder {
    fn topics(&self) -> Vec<String> {
        self.published
            .lock()
            .unwrap()
            .iter()
            .map(|publication| publication.topic.clone())
            .collect()
    }

    /// The decoded payload of the one publication on `topic`.
    fn payload(&self, topic: &str) -> Json {
        let published = self.published.lock().unwrap();
        // Built before the lookup, not inside the panic: a `self.topics`
        // there would take the same lock a second time and hang the test
        // instead of failing it.
        let seen: Vec<&str> = published
            .iter()
            .map(|publication| publication.topic.as_str())
            .collect();
        let mut matching = published
            .iter()
            .filter(|publication| publication.topic == topic);
        let publication = matching
            .next()
            .unwrap_or_else(|| panic!("nothing published on {topic}; saw {seen:?}"));
        assert!(
            matching.next().is_none(),
            "{topic} was published more than once"
        );
        serde_json::from_slice(&publication.payload).expect("payload is JSON")
    }

    /// The decoded payload of the most recent publication on `topic`, for
    /// topics a test expects more than one of.
    fn last_payload(&self, topic: &str) -> Json {
        let published = self.published.lock().unwrap();
        let publication = published
            .iter()
            .rev()
            .find(|publication| publication.topic == topic)
            .unwrap_or_else(|| panic!("nothing published on {topic}"));
        serde_json::from_slice(&publication.payload).expect("payload is JSON")
    }

    fn count(&self, topic: &str) -> usize {
        self.published
            .lock()
            .unwrap()
            .iter()
            .filter(|publication| publication.topic == topic)
            .count()
    }

    fn clear(&self) {
        self.published.lock().unwrap().clear();
    }
}

/// An [`ItemSource`] that records writes instead of making them.
struct Fake {
    items: BTreeMap<String, Item>,
    writes: Mutex<Vec<(String, String, Json)>>,
    outcome: WriteOutcome,
}

impl Fake {
    fn new() -> Self {
        Self {
            items: tree(),
            writes: Mutex::new(Vec::new()),
            outcome: WriteOutcome::Accepted,
        }
    }

    fn writes(&self) -> Vec<(String, String, Json)> {
        self.writes.lock().unwrap().clone()
    }
}

#[async_trait]
impl ItemSource for Fake {
    async fn get_items(
        &self,
        application: &topic::Application,
    ) -> anyhow::Result<BTreeMap<String, Item>> {
        assert_eq!(application.bus_name(), APPLICATION_SERVICE);
        Ok(self.items.clone())
    }

    async fn set_value(
        &self,
        application: &topic::Application,
        path: &str,
        value: Json,
    ) -> WriteOutcome {
        self.writes.lock().unwrap().push((
            application.bus_name().to_string(),
            path.to_string(),
            value,
        ));
        self.outcome.clone()
    }
}

/// Bridge, transport and source wired exactly as the daemon wires them.
struct Harness {
    bridge: Bridge,
    transport: Recorder,
    source: Fake,
}

impl Harness {
    fn new(mode: Mode) -> Self {
        Self {
            bridge: Bridge::new(DEVICE, mode, Timings::default()),
            transport: Recorder::default(),
            source: Fake::new(),
        }
    }

    /// Carry out one event's effects through the production [`apply`].
    async fn run(&self, effects: Effects) {
        apply(effects, &self.transport, &self.source)
            .await
            .expect("effects applied");
    }

    /// The startup path: `GetItems` into the mirror, then a keepalive to open
    /// the alive window every later publication is gated on.
    async fn start(&mut self, now: Duration) {
        let application = application();
        let items = self.source.get_items(&application).await.expect("seed");
        let effects = self.bridge.upsert_service(now, application, items);
        self.run(effects).await;
        let effects = self.bridge.on_keepalive(now);
        self.run(effects).await;
    }
}
