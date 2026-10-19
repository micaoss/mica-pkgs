//! BlueZ over D-Bus (protocol section 1): the LE advertisement and the GATT
//! application with `rx` and `tx`. One phone at a time: the advertisement is
//! withdrawn while a phone is connected, and any other device that writes is
//! disconnected.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use zbus::{Connection, MatchRule, MessageStream, Proxy, interface};
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

use crate::framing::{self, Reassembler};
use crate::session::{Device, Session, Step};

pub const SERVICE_UUID: &str = "ab725d4b-fef6-4311-99ec-a26c3139fc13";
pub const RX_UUID: &str = "95468405-fe96-45be-9ea0-769fc0e27237";
pub const TX_UUID: &str = "29358014-0d71-4c66-9a54-49c3943742ea";

const APP_PATH: &str = "/com/mica/bled";
const SERVICE_PATH: &str = "/com/mica/bled/service0";
const RX_PATH: &str = "/com/mica/bled/service0/rx";
const TX_PATH: &str = "/com/mica/bled/service0/tx";
const ADV_PATH: &str = "/com/mica/bled/advertisement0";

/// How long a phone may stay connected without opening a session.
const AUTH_DEADLINE: Duration = Duration::from_secs(30);
/// The ATT MTU assumed until BlueZ reports one.
const DEFAULT_MTU: u16 = 23;

enum Event {
    Write {
        device: OwnedObjectPath,
        value: Vec<u8>,
        mtu: Option<u16>,
    },
    Disconnected(OwnedObjectPath),
}

fn path(p: &'static str) -> OwnedObjectPath {
    OwnedObjectPath::from(ObjectPath::from_static_str_unchecked(p))
}

struct GattService;

#[interface(name = "org.bluez.GattService1")]
impl GattService {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        SERVICE_UUID.to_string()
    }

    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
}

struct RxCharacteristic {
    events: mpsc::UnboundedSender<Event>,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl RxCharacteristic {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        RX_UUID.to_string()
    }

    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        path(SERVICE_PATH)
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["write".into(), "write-without-response".into()]
    }

    // Not async: the chunk is queued before the next call is dispatched, so
    // the order of the writes is kept.
    fn write_value(&self, value: Vec<u8>, options: HashMap<String, OwnedVariant>) {
        let Some(device) = options
            .get("device")
            .and_then(|v| v.downcast_ref::<ObjectPath<'_>>().ok())
            .map(OwnedObjectPath::from)
        else {
            return;
        };
        let mtu = options
            .get("mtu")
            .and_then(|v| v.downcast_ref::<u16>().ok());
        let _ = self.events.send(Event::Write { device, value, mtu });
    }
}

#[derive(Default)]
struct TxCharacteristic {
    value: Vec<u8>,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl TxCharacteristic {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        TX_UUID.to_string()
    }

    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        path(SERVICE_PATH)
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["notify".into()]
    }

    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        self.value.clone()
    }

    fn start_notify(&self) {}

    fn stop_notify(&self) {}
}

struct Advertisement;

#[interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    fn release(&self) {}

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".to_string()
    }

    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec![SERVICE_UUID.to_string()]
    }

    #[zbus(property)]
    fn includes(&self) -> Vec<String> {
        vec!["local-name".to_string()]
    }

    // The adapter itself stays undiscoverable; without the LE General
    // Discoverable flag in this advertisement, phone scans filter it out.
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }
}

/// The first adapter BlueZ offers GATT on, waiting until there is one.
async fn find_adapter(conn: &Connection) -> anyhow::Result<OwnedObjectPath> {
    loop {
        let manager = zbus::fdo::ObjectManagerProxy::builder(conn)
            .destination("org.bluez")
            .path("/")
            .build()
            .await?;
        match manager.get_managed_objects().await {
            Ok(objects) => {
                let mut adapters: Vec<_> = objects
                    .into_iter()
                    .filter(|(_, ifaces)| {
                        ifaces
                            .keys()
                            .any(|i| i.as_str() == "org.bluez.GattManager1")
                            && ifaces
                                .keys()
                                .any(|i| i.as_str() == "org.bluez.LEAdvertisingManager1")
                    })
                    .map(|(p, _)| p)
                    .collect();
                adapters.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                if let Some(adapter) = adapters.into_iter().next() {
                    return Ok(adapter);
                }
                tracing::info!("no Bluetooth adapter yet");
            }
            Err(err) => tracing::info!("BlueZ is not answering yet: {err}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// `method(target, {})` on a BlueZ manager object.
async fn register(
    conn: &Connection,
    adapter: &OwnedObjectPath,
    iface: &'static str,
    method: &'static str,
    target: &'static str,
) -> zbus::Result<()> {
    let options: HashMap<&str, Variant<'_>> = HashMap::new();
    Proxy::new(conn, "org.bluez", adapter.as_str(), iface)
        .await?
        .call_method(
            method,
            &(ObjectPath::from_static_str_unchecked(target), options),
        )
        .await?;
    Ok(())
}

async fn advertise(conn: &Connection, adapter: &OwnedObjectPath, on: bool) {
    const IFACE: &str = "org.bluez.LEAdvertisingManager1";
    let result = if on {
        register(conn, adapter, IFACE, "RegisterAdvertisement", ADV_PATH).await
    } else {
        async {
            Proxy::new(conn, "org.bluez", adapter.as_str(), IFACE)
                .await?
                .call_method(
                    "UnregisterAdvertisement",
                    &(ObjectPath::from_static_str_unchecked(ADV_PATH),),
                )
                .await
                .map(drop)
        }
        .await
    };
    if let Err(err) = result {
        tracing::warn!("advertising {}: {err}", if on { "on" } else { "off" });
    }
}

async fn disconnect(conn: &Connection, device: &OwnedObjectPath) {
    let result = async {
        Proxy::new(conn, "org.bluez", device.as_str(), "org.bluez.Device1")
            .await?
            .call_method("Disconnect", &())
            .await
    }
    .await;
    if let Err(err) = result {
        tracing::warn!("disconnecting {}: {err}", device.as_str());
    }
}

/// The adapter's name, which BlueZ also advertises.
async fn adapter_alias(conn: &Connection, adapter: &OwnedObjectPath) -> String {
    let alias = async {
        Proxy::new(conn, "org.bluez", adapter.as_str(), "org.bluez.Adapter1")
            .await?
            .get_property::<String>("Alias")
            .await
    }
    .await;
    alias.unwrap_or_default()
}

/// Forward `Connected = false` of any BlueZ device; fail when BlueZ leaves
/// the bus, since every registration went with it.
async fn watch(conn: Connection, events: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")
        .interface("org.freedesktop.DBus.Properties")
        .member("PropertiesChanged")
        .arg(0, "org.bluez.Device1")
        .build()?;
    let mut changes = MessageStream::for_match_rule(rule, &conn, None).await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.bluez")])
        .await?;
    loop {
        tokio::select! {
            msg = changes.next() => {
                let Some(Ok(msg)) = msg else { anyhow::bail!("the D-Bus connection ended") };
                let Some(device) = msg.header().path().map(|p| OwnedObjectPath::from(p.clone())) else {
                    continue;
                };
                let body = msg.body();
                let Ok((_, changed, _)) =
                    body.deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
                else {
                    continue;
                };
                if changed.get("Connected").and_then(|v| v.downcast_ref::<bool>().ok()) == Some(false) {
                    let _ = events.send(Event::Disconnected(device));
                }
            }
            change = owners.next() => {
                let Some(change) = change else { anyhow::bail!("the D-Bus connection ended") };
                if change.args().is_ok_and(|a| a.new_owner().is_none()) {
                    anyhow::bail!("bluetoothd left the bus");
                }
            }
        }
    }
}

struct Current {
    device: OwnedObjectPath,
    session: Session,
    stream: Reassembler,
    mtu: u16,
    deadline: Instant,
}

/// Serve phones until BlueZ or the bus goes away.
pub async fn run(conn: Connection, device: Arc<Device>) -> anyhow::Result<()> {
    let (events, mut queue) = mpsc::unbounded_channel();
    let server = conn.object_server();
    server.at(APP_PATH, zbus::fdo::ObjectManager).await?;
    server.at(SERVICE_PATH, GattService).await?;
    server
        .at(
            RX_PATH,
            RxCharacteristic {
                events: events.clone(),
            },
        )
        .await?;
    server.at(TX_PATH, TxCharacteristic::default()).await?;
    server.at(ADV_PATH, Advertisement).await?;
    let tx = server.interface::<_, TxCharacteristic>(TX_PATH).await?;

    let adapter = find_adapter(&conn).await?;
    let mut watcher = tokio::spawn(watch(conn.clone(), events));
    register(
        &conn,
        &adapter,
        "org.bluez.GattManager1",
        "RegisterApplication",
        APP_PATH,
    )
    .await
    .context("registering the GATT application")?;
    advertise(&conn, &adapter, true).await;
    tracing::info!(adapter = adapter.as_str(), "serving");

    let mut current: Option<Current> = None;
    loop {
        let deadline = current
            .as_ref()
            .filter(|c| !c.session.is_open())
            .map(|c| c.deadline);
        let event = tokio::select! {
            event = queue.recv() => event,
            () = sleep_until(deadline) => {
                if let Some(c) = current.take() {
                    tracing::info!("{}: no session after {AUTH_DEADLINE:?}", c.device.as_str());
                    disconnect(&conn, &c.device).await;
                    advertise(&conn, &adapter, true).await;
                }
                continue;
            }
            result = &mut watcher => {
                return match result {
                    Ok(Err(err)) => Err(err),
                    Ok(Ok(())) => Ok(()),
                    Err(err) => Err(err.into()),
                };
            }
        };
        let Some(event) = event else {
            return Ok(());
        };
        match event {
            Event::Disconnected(gone) => {
                if current.as_ref().is_some_and(|c| c.device == gone) {
                    tracing::info!("{}: disconnected", gone.as_str());
                    current = None;
                    advertise(&conn, &adapter, true).await;
                }
            }
            Event::Write {
                device: from,
                value,
                mtu,
            } => {
                if current.as_ref().is_some_and(|c| c.device != from) {
                    tracing::info!("{}: busy, refused", from.as_str());
                    disconnect(&conn, &from).await;
                    continue;
                }
                let c = match &mut current {
                    Some(c) => c,
                    None => {
                        tracing::info!("{}: connected", from.as_str());
                        advertise(&conn, &adapter, false).await;
                        let name = adapter_alias(&conn, &adapter).await;
                        current.insert(Current {
                            device: from,
                            session: Session::new(device.clone(), name),
                            stream: Reassembler::default(),
                            mtu: DEFAULT_MTU,
                            deadline: Instant::now() + AUTH_DEADLINE,
                        })
                    }
                };
                if let Some(mtu) = mtu {
                    c.mtu = mtu;
                }
                let closed = match c.stream.push(&value) {
                    Ok(messages) => {
                        let mut closed = false;
                        for msg in messages {
                            for step in c.session.handle(&msg).await {
                                match step {
                                    Step::Send(out) => notify(&tx, &out, c.mtu).await,
                                    Step::Close => closed = true,
                                }
                            }
                            if closed {
                                break;
                            }
                        }
                        closed
                    }
                    Err(err) => {
                        tracing::info!("{}: {err}", c.device.as_str());
                        true
                    }
                };
                if closed && let Some(c) = current.take() {
                    disconnect(&conn, &c.device).await;
                    advertise(&conn, &adapter, true).await;
                }
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Send one message as notifications of at most `mtu - 3` bytes.
async fn notify(
    tx: &zbus::object_server::InterfaceRef<TxCharacteristic>,
    message: &[u8],
    mtu: u16,
) {
    let Ok(stream) = framing::encode(message) else {
        return;
    };
    for chunk in framing::chunks(&stream, mtu) {
        let mut iface = tx.get_mut().await;
        iface.value = chunk.to_vec();
        if let Err(err) = iface.value_changed(tx.signal_emitter()).await {
            tracing::warn!("notify: {err}");
            return;
        }
    }
}
