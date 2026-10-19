//! MQTT and D-Bus I/O around the application-only protocol state machine.

use crate::bridge::{Bridge, Effects, Write};
use crate::config::{Mode, Timings};
use crate::enrollment::Enrollment;
use crate::source::{
    APPLICATION_CALL_TIMEOUT, Bounded, BusSource, ItemSource, ItemTreeProxy, batch_of,
};
use crate::topic::{self, Application};
use crate::transport::{MqttTransport, Transport};
use futures_util::StreamExt;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use zbus::{MatchRule, MessageStream};

mod activation;
use activation::*;

/// Carry out one event's broker publications and application writes.
pub async fn apply(
    effects: Effects,
    transport: &dyn Transport,
    source: &dyn ItemSource,
) -> anyhow::Result<()> {
    for publication in &effects.publications {
        transport.publish(publication).await?;
    }
    for Write {
        application,
        path,
        value,
    } in &effects.writes
    {
        let outcome = source.set_value(application, path, value.clone()).await;
        if outcome.accepted() {
            tracing::info!(
                application = application.bus_name(),
                path,
                "write request carried through to application SetValue"
            );
        } else {
            tracing::warn!(
                application = application.bus_name(),
                path,
                outcome = ?outcome,
                "application write request was not carried out"
            );
        }
    }
    Ok(())
}

/// Everything the daemon is told at startup.
pub struct Settings {
    pub device_id: String,
    pub applications_dir: PathBuf,
    pub broker_host: String,
    pub broker_port: u16,
    pub client_id: String,
    /// Optional-on-disk JSON credentials, readable only by the bridge account.
    pub credentials_file: PathBuf,
    pub mode: Mode,
    pub session_bus: bool,
    pub timings: Timings,
}

/// Construct the broker connection used by the runtime and integration tests.
pub fn mqtt_options(settings: &Settings) -> anyhow::Result<MqttOptions> {
    let mut options = MqttOptions::new(
        settings.client_id.clone(),
        settings.broker_host.clone(),
        settings.broker_port,
    );
    options.set_keep_alive(Duration::from_secs(30));
    if let Some((username, password)) =
        crate::config::broker_credentials(&settings.credentials_file)?
    {
        options.set_credentials(username, password);
    }
    Ok(options)
}

const REQUEST_CAPACITY: usize = 64;
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long after a failed activation the bus is swept again.
///
/// An application can fail its first `GetItems` and still own its name: one
/// that claims the name before it registers `/`, or one whose `ItemsChanged`
/// stream ended. No `NameOwnerChanged` will follow, so the runtime comes
/// back on its own rather than waiting for a restart.
const ACTIVATION_RETRY: Duration = Duration::from_secs(5);

/// Doubling broker reconnect backoff with a floor and ceiling.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectBackoff {
    next: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            next: RECONNECT_BACKOFF_MIN,
        }
    }
}

impl ReconnectBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(RECONNECT_BACKOFF_MAX);
        delay
    }

    pub fn reset(&mut self) {
        self.next = RECONNECT_BACKOFF_MIN;
    }
}

/// A message on a subscribed topic, handed from the event-loop task to the
/// runtime.
struct Incoming {
    topic: String,
    payload: Vec<u8>,
}

/// Hand one broker message to the runtime without waiting for it.
fn forward(tx: &mpsc::Sender<Incoming>, message: Incoming) -> bool {
    match tx.try_send(message) {
        Ok(()) => true,
        Err(TrySendError::Full(dropped)) => {
            tracing::warn!(
                topic = dropped.topic,
                "runtime is busy; dropping the broker request"
            );
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

enum ApplicationEvent {
    Items {
        bus_name: String,
        generation: u64,
        items: BTreeMap<String, Option<crate::item::Item>>,
    },
    WatcherStopped {
        bus_name: String,
        generation: u64,
        detail: String,
    },
}

struct ActiveApplication {
    owner: String,
    generation: u64,
    watcher: JoinHandle<()>,
}

impl Drop for ActiveApplication {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

fn merge(mut left: Effects, right: Effects) -> Effects {
    left.publications.extend(right.publications);
    left.writes.extend(right.writes);
    left
}

/// Apply one application event. The flag asks the runtime to sweep the bus
/// again: a watcher that stopped while its owner is still there is an
/// application worth re-activating.
fn handle_application_event(
    now: Duration,
    bridge: &mut Bridge,
    active: &mut BTreeMap<String, ActiveApplication>,
    event: ApplicationEvent,
) -> (Effects, bool) {
    match event {
        ApplicationEvent::Items {
            bus_name,
            generation,
            items,
        } => {
            if active
                .get(&bus_name)
                .is_some_and(|current| current.generation == generation)
            {
                (bridge.on_items_changed(now, &bus_name, items), false)
            } else {
                (Effects::default(), false)
            }
        }
        ApplicationEvent::WatcherStopped {
            bus_name,
            generation,
            detail,
        } => {
            if active
                .get(&bus_name)
                .is_some_and(|current| current.generation == generation)
            {
                active.remove(&bus_name);
                tracing::warn!(
                    application = bus_name,
                    error = detail,
                    "application ItemsChanged watcher stopped; withdrawing stale MQTT state"
                );
                (bridge.on_service_vanished(now, &bus_name), true)
            } else {
                (Effects::default(), false)
            }
        }
    }
}

/// Match ownership changes in the mica namespace.
///
/// The signal comes from the bus daemon, not from the named service. The
/// runtime checks the exact enrollment before asking for an owner or creating
/// an Item1 proxy, so an unregistered service such as micad is observed only as
/// a string and is never called or subscribed to.
fn mica_owner_rule() -> zbus::Result<MatchRule<'static>> {
    MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .arg0ns(mica_busname::PREFIX.trim_end_matches('.'))
        .build()
}

async fn watch_application(
    connection: zbus::Connection,
    application: Application,
    generation: u64,
    ready: oneshot::Sender<Result<(), String>>,
    tx: mpsc::Sender<ApplicationEvent>,
) -> anyhow::Result<()> {
    let proxy = match ItemTreeProxy::builder(&connection)
        .destination(application.bus_name().to_string())
        .build()
        .await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return Err(err.into());
        }
    };
    let mut changes = match proxy.receive_items_changed().await {
        Ok(changes) => changes,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return Err(err.into());
        }
    };
    let _ = ready.send(Ok(()));
    while let Some(signal) = changes.next().await {
        let items = batch_of(signal.args()?.items);
        if tx
            .send(ApplicationEvent::Items {
                bus_name: application.bus_name().to_string(),
                generation,
                items,
            })
            .await
            .is_err()
        {
            return Ok(());
        }
    }
    anyhow::bail!("ItemsChanged stream ended")
}

/// Connect to enrolled application services and the broker, then run until
/// the process is asked to stop.
pub async fn run(settings: Settings) -> anyhow::Result<()> {
    if !topic::valid_topic_segment(&settings.device_id) {
        anyhow::bail!("configured device id cannot form an MQTT topic segment");
    }
    let enrollment = Enrollment::load(&settings.applications_dir).await?;
    let connection = if settings.session_bus {
        zbus::Connection::session().await?
    } else {
        zbus::Connection::system().await?
    };
    let source = Bounded::new(BusSource::new(connection.clone()), APPLICATION_CALL_TIMEOUT);

    let owner_rule = mica_owner_rule()?;
    let mut owners = Box::pin(MessageStream::for_match_rule(owner_rule, &connection, None).await?);
    let bus = zbus::fdo::DBusProxy::new(&connection).await?;

    let options = mqtt_options(&settings)?;
    let (client, mut eventloop) = AsyncClient::new(options, REQUEST_CAPACITY);
    let transport = MqttTransport::new(client);

    // Two channels out of the event-loop task, neither of which it waits on:
    // requests are dropped when the runtime is busy (see `forward`), and a
    // connection is a counter the runtime catches up with when it can, so a
    // reconnect is never lost behind a queue of requests.
    let (incoming_tx, mut incoming) = mpsc::channel(REQUEST_CAPACITY);
    let (connected_tx, mut connected) = watch::channel(0u64);
    tokio::spawn(async move {
        let mut backoff = ReconnectBackoff::default();
        loop {
            let event = match eventloop.poll().await {
                Ok(event) => {
                    backoff.reset();
                    event
                }
                Err(err) => {
                    let delay = backoff.next_delay();
                    tracing::warn!(
                        error = %err,
                        retry_in_s = delay.as_secs(),
                        "broker connection lost; retrying after backoff"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            match event {
                Event::Incoming(Packet::Publish(publish)) => {
                    let message = Incoming {
                        topic: publish.topic,
                        payload: publish.payload.to_vec(),
                    };
                    if !forward(&incoming_tx, message) {
                        return;
                    }
                }
                Event::Incoming(Packet::ConnAck(_)) => {
                    connected_tx.send_modify(|count| *count += 1);
                }
                _ => {}
            }
        }
    });

    let (changes_tx, mut changes) = mpsc::channel(REQUEST_CAPACITY);
    let mut activation = Activation {
        connection: &connection,
        source: &source,
        changes_tx: &changes_tx,
        active: BTreeMap::new(),
        next_generation: 0,
    };
    let mut bridge = Bridge::new(settings.device_id, settings.mode, settings.timings);
    let mut subscribed = Vec::new();
    let start = Instant::now();
    let mut resweep_at: Option<Instant> = None;

    // The ownership match is installed before this sweep, so a service that
    // appears during it is either listed or queued as a signal (possibly
    // both; owner equality makes the duplicate harmless).
    if activation
        .sweep(&bus, &enrollment, &mut bridge, &transport, start)
        .await?
    {
        resweep_at = Some(Instant::now() + ACTIVATION_RETRY);
    }
    resubscribe(&bridge, &transport, &mut subscribed).await?;

    loop {
        let now = start.elapsed();
        let wake = bridge.next_wake(now).map(|wake| start + wake);
        let effects = tokio::select! {
            event = changes.recv() => {
                let Some(event) = event else { return Ok(()) };
                let (effects, resweep) =
                    handle_application_event(start.elapsed(), &mut bridge, &mut activation.active, event);
                if resweep {
                    resweep_at.get_or_insert(Instant::now() + ACTIVATION_RETRY);
                }
                effects
            }
            owner = owners.next() => {
                let Some(owner) = owner else { return Ok(()) };
                let owner = owner?;
                let (name, _old_owner, new_owner) =
                    owner.body().deserialize::<(String, String, String)>()?;
                let Some(application) = enrollment.application(&name) else {
                    continue;
                };
                if new_owner.is_empty() {
                    if activation.active.remove(&name).is_some() {
                        tracing::info!(application = name, "application left the bus; clearing retained MQTT state");
                        bridge.on_service_vanished(start.elapsed(), &name)
                    } else {
                        Effects::default()
                    }
                } else {
                    let effects = activation
                        .activate(&mut bridge, start.elapsed(), application, new_owner)
                        .await;
                    if !activation.active.contains_key(&name) {
                        resweep_at.get_or_insert(Instant::now() + ACTIVATION_RETRY);
                    }
                    effects
                }
            }
            () = sleep_until(resweep_at) => {
                resweep_at = None;
                if activation
                    .sweep(&bus, &enrollment, &mut bridge, &transport, start)
                    .await?
                {
                    resweep_at = Some(Instant::now() + ACTIVATION_RETRY);
                }
                Effects::default()
            }
            message = incoming.recv() => {
                let Some(Incoming { topic, payload }) = message else { return Ok(()) };
                bridge.on_request(start.elapsed(), &topic, &payload)
            }
            changed = connected.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                subscribed.clear();
                Effects::default()
            }
            () = sleep_until(wake) => bridge.on_tick(start.elapsed()),
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        apply(effects, &transport, &source).await?;
        resubscribe(&bridge, &transport, &mut subscribed).await?;
    }
}

async fn resubscribe(
    bridge: &Bridge,
    transport: &dyn Transport,
    subscribed: &mut Vec<String>,
) -> anyhow::Result<()> {
    let wanted = bridge.subscriptions();
    if wanted == *subscribed {
        return Ok(());
    }
    for filter in subscribed.iter().filter(|filter| !wanted.contains(filter)) {
        transport.unsubscribe(filter).await?;
    }
    for filter in wanted.iter().filter(|filter| !subscribed.contains(filter)) {
        tracing::info!(filter, "subscribing");
        transport.subscribe(filter).await?;
    }
    *subscribed = wanted;
    Ok(())
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
