//! mica-bled against a fake BlueZ on a private dbus-daemon: the objects it
//! exports (as BlueZ reads them with GetManagedObjects and GetAll, and their
//! introspection), the calls it makes to BlueZ, the notifications it sends on
//! `tx`, and the signals it follows. What goes over the bus is compared with
//! the files under tests/bluez/; MICA_BLESS=1 rewrites them.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use mica_bled::apid::{Apid, Reply, Unavailable};
use mica_bled::phones::Phones;
use mica_bled::session::Device;
use serde_json::Value as Json;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, MatchRule, MessageStream, Proxy};
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Structure, Variant};

const ADAPTER: &str = "/org/bluez/hci0";
const PHONE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const PATHS: [(&str, &str); 6] = [
    ("app", "/com/mica/bled"),
    ("service", "/com/mica/bled/service0"),
    ("rx", "/com/mica/bled/service0/rx"),
    ("tx", "/com/mica/bled/service0/tx"),
    ("advertisement", "/com/mica/bled/advertisement0"),
    ("root", "/"),
];

/// A dbus-daemon of the session configuration on a socket of its own.
struct Bus {
    daemon: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl Bus {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(format!("--address={address}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("dbus-daemon runs");
        let bus = Self {
            daemon,
            address,
            _dir: dir,
        };
        for _ in 0..500 {
            if socket.exists() {
                return bus;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("dbus-daemon did not listen on {}", socket.display());
    }

    async fn connect(&self) -> Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .build()
            .await
            .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// A value with its D-Bus type, dictionaries in key order.
fn render(value: &Variant<'_>) -> String {
    let inner = match value {
        Variant::Bool(v) => v.to_string(),
        Variant::U8(v) => v.to_string(),
        Variant::I16(v) => v.to_string(),
        Variant::U16(v) => v.to_string(),
        Variant::I32(v) => v.to_string(),
        Variant::U32(v) => v.to_string(),
        Variant::I64(v) => v.to_string(),
        Variant::U64(v) => v.to_string(),
        Variant::F64(v) => v.to_string(),
        Variant::Str(v) => format!("{:?}", v.as_str()),
        Variant::Signature(v) => format!("{:?}", v.to_string()),
        Variant::ObjectPath(v) => format!("{:?}", v.as_str()),
        Variant::Variant(v) => format!("<{}>", render(v)),
        Variant::Array(items) => {
            let items: Vec<_> = items.iter().map(render).collect();
            format!("[{}]", items.join(", "))
        }
        Variant::Dict(dict) => {
            let mut entries: Vec<_> = dict
                .iter()
                .map(|(k, v)| format!("{}: {}", render(k), render(v)))
                .collect();
            entries.sort();
            format!("{{{}}}", entries.join(", "))
        }
        Variant::Structure(fields) => {
            let fields: Vec<_> = fields.fields().iter().map(render).collect();
            format!("({})", fields.join(", "))
        }
        #[allow(unreachable_patterns)]
        other => panic!("unexpected value {other:?}"),
    };
    format!("{} {inner}", value.value_signature())
}

fn render_options(options: &HashMap<String, OwnedVariant>) -> String {
    let mut entries: Vec<_> = options
        .iter()
        .map(|(k, v)| format!("{k:?}: {}", render(v)))
        .collect();
    entries.sort();
    format!("{{{}}}", entries.join(", "))
}

/// Introspection XML with sibling `<node>` blocks after the node's own lines,
/// in name order: zbus lists children in no fixed order.
fn canonical_xml(xml: &str) -> String {
    fn canon(lines: &[&str]) -> Vec<String> {
        let mut own = Vec::new();
        let mut children: Vec<Vec<String>> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i].trim();
            if line.starts_with("<node name=") && !line.ends_with("/>") {
                let mut depth = 0;
                let mut end = i;
                for (j, l) in lines.iter().enumerate().skip(i) {
                    let l = l.trim();
                    if l.starts_with("<node") && !l.ends_with("/>") {
                        depth += 1;
                    } else if l == "</node>" {
                        depth -= 1;
                        if depth == 0 {
                            end = j;
                            break;
                        }
                    }
                }
                let mut block = vec![line.to_string()];
                block.extend(canon(&lines[i + 1..end]));
                block.push("</node>".to_string());
                children.push(block);
                i = end + 1;
            } else if line.starts_with("<node name=") {
                children.push(vec![line.to_string()]);
                i += 1;
            } else {
                own.push(line.to_string());
                i += 1;
            }
        }
        children.sort();
        own.extend(children.into_iter().flatten());
        own
    }
    let lines: Vec<&str> = xml.lines().filter(|l| !l.trim().is_empty()).collect();
    canon(&lines).join("\n") + "\n"
}

/// The fixture `name`, or with MICA_BLESS=1 the fixture rewritten.
fn check(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/bluez")
        .join(name);
    if std::env::var_os("MICA_BLESS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        expected == actual,
        "{} differs from the bus:\n--- expected\n{expected}\n--- actual\n{actual}",
        path.display()
    );
}

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, line: String) {
    log.lock().unwrap().push(line);
}

/// Until BlueZ has seen `count` calls whose line starts with `call`.
async fn wait_for(log: &Log, call: &str, count: usize) {
    for _ in 0..500 {
        if log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(call))
            .count()
            >= count
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "BlueZ saw {:?}, expected {count} {call}",
        log.lock().unwrap()
    );
}

// The fake BlueZ answers with the argument types BlueZ declares, so a call of
// another signature fails; what was called is recorded off the bus by
// `record`, not by these handlers.
struct GattManager;

#[zbus::interface(name = "org.bluez.GattManager1")]
impl GattManager {
    fn register_application(&self, _app: OwnedObjectPath, _options: HashMap<String, OwnedVariant>) {
    }
}

struct AdvertisingManager;

#[zbus::interface(name = "org.bluez.LEAdvertisingManager1")]
impl AdvertisingManager {
    fn register_advertisement(
        &self,
        _advertisement: OwnedObjectPath,
        _options: HashMap<String, OwnedVariant>,
    ) {
    }

    fn unregister_advertisement(&self, _advertisement: OwnedObjectPath) {}
}

struct Adapter;

#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn alias(&self) -> String {
        "mica-1234".to_string()
    }
}

struct Phone;

#[zbus::interface(name = "org.bluez.Device1")]
impl Phone {
    fn disconnect(&self) {}
}

/// Every method call `from` makes to `conn`: interface, member, path and
/// body with its signature.
async fn record(conn: Connection, from: String, log: Log) {
    let mut stream = MessageStream::from(&conn);
    while let Some(Ok(msg)) = stream.next().await {
        let header = msg.header();
        if msg.message_type() != zbus::message::Type::MethodCall
            || header.sender().map(|s| s.as_str()) != Some(from.as_str())
        {
            continue;
        }
        let body = msg.body();
        let args = if body.signature().to_string().is_empty() {
            "()".to_string()
        } else {
            render(&Variant::from(body.deserialize::<Structure<'_>>().unwrap()))
        };
        push(
            &log,
            format!(
                "{}.{} {} {args}",
                header.interface().map(|i| i.as_str()).unwrap_or_default(),
                header.member().map(|m| m.as_str()).unwrap_or_default(),
                header.path().map(|p| p.as_str()).unwrap_or_default(),
            ),
        );
    }
}

struct NoApid;

#[async_trait::async_trait]
impl Apid for NoApid {
    async fn request(&self, _: &str, _: &str, _: Option<&Json>) -> Result<Reply, Unavailable> {
        Err(Unavailable("not in this test".into()))
    }
}

/// A `WriteValue` on `rx` as BlueZ makes it for a phone.
async fn write(conn: &Connection, bled: &str, value: &[u8], mtu: u16) {
    let options: HashMap<&str, Variant<'_>> = HashMap::from([
        (
            "device",
            Variant::from(ObjectPath::try_from(PHONE).unwrap()),
        ),
        ("mtu", Variant::from(mtu)),
        ("link", Variant::from("LE")),
    ]);
    Proxy::new(
        conn,
        bled,
        "/com/mica/bled/service0/rx",
        "org.bluez.GattCharacteristic1",
    )
    .await
    .unwrap()
    .call_method("WriteValue", &(value, options))
    .await
    .unwrap();
}

#[tokio::test]
async fn bled_on_the_bus_as_bluez_sees_it() {
    let bus = Bus::start();
    let log: Log = Arc::default();

    let bluez = bus.connect().await;
    let server = bluez.object_server();
    server.at("/", zbus::fdo::ObjectManager).await.unwrap();
    server.at(ADAPTER, GattManager).await.unwrap();
    server.at(ADAPTER, AdvertisingManager).await.unwrap();
    server.at(ADAPTER, Adapter).await.unwrap();
    server.at(PHONE, Phone).await.unwrap();
    bluez.request_name("org.bluez").await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let phones = Phones::load(&dir.path().join("phones.json")).unwrap();
    let device = Arc::new(Device::new(
        "00112233445566778899aabbccddeeff".into(),
        Arc::new(NoApid),
        phones,
    ));
    let conn = bus.connect().await;
    let bled = conn.unique_name().unwrap().to_string();
    tokio::spawn(record(bluez.clone(), bled.clone(), log.clone()));
    let run = tokio::spawn(mica_bled::gatt::run(conn, device));
    wait_for(
        &log,
        "org.bluez.LEAdvertisingManager1.RegisterAdvertisement",
        1,
    )
    .await;

    // What BlueZ reads of the application and the advertisement.
    let probe = bus.connect().await;
    let manager = zbus::fdo::ObjectManagerProxy::builder(&probe)
        .destination(bled.as_str())
        .path("/com/mica/bled")
        .build()
        .await
        .unwrap();
    let mut objects = Vec::new();
    for (path, ifaces) in manager.get_managed_objects().await.unwrap() {
        for (iface, props) in ifaces {
            objects.push(format!(
                "{} {} {}",
                path.as_str(),
                iface.as_str(),
                render_options(&props)
            ));
        }
    }
    objects.sort();
    check("managed-objects.txt", &(objects.join("\n") + "\n"));

    let properties = zbus::fdo::PropertiesProxy::builder(&probe)
        .destination(bled.as_str())
        .path("/com/mica/bled/advertisement0")
        .build()
        .await
        .unwrap();
    let iface = zbus::names::InterfaceName::try_from("org.bluez.LEAdvertisement1").unwrap();
    let all = properties.get_all(iface).await.unwrap();
    check("advertisement.txt", &(render_options(&all) + "\n"));

    for (name, path) in PATHS {
        let xml = zbus::fdo::IntrospectableProxy::builder(&probe)
            .destination(bled.as_str())
            .path(path)
            .build()
            .await
            .unwrap()
            .introspect()
            .await
            .unwrap();
        check(&format!("introspect-{name}.xml"), &canonical_xml(&xml));
    }

    // A phone writes a malformed message: bled answers on tx and hangs up.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(bled.as_str())
        .path("/com/mica/bled/service0/tx")
        .interface("org.freedesktop.DBus.Properties")
        .member("PropertiesChanged")
        .build()
        .unwrap();
    let mut notifications = MessageStream::for_match_rule(rule, &probe, None)
        .await
        .unwrap();
    Proxy::new(
        &probe,
        bled.as_str(),
        "/com/mica/bled/service0/tx",
        "org.bluez.GattCharacteristic1",
    )
    .await
    .unwrap()
    .call_method("StartNotify", &())
    .await
    .unwrap();
    write(&probe, &bled, &[0, 0, 0, 1, b'x'], 23).await;
    wait_for(
        &log,
        "org.bluez.LEAdvertisingManager1.RegisterAdvertisement",
        2,
    )
    .await;
    let mut received = Vec::new();
    let mut message = Vec::new();
    while message.len() < 4
        || message.len() < 4 + u32::from_be_bytes(message[..4].try_into().unwrap()) as usize
    {
        let msg = tokio::time::timeout(Duration::from_secs(5), notifications.next())
            .await
            .expect("a notification")
            .unwrap()
            .unwrap();
        let body = msg.body();
        let (iface, changed, invalidated): (String, HashMap<String, OwnedVariant>, Vec<String>) =
            body.deserialize().unwrap();
        received.push(format!(
            "{} {iface} {} {invalidated:?}",
            msg.body().signature(),
            render_options(&changed)
        ));
        let chunk: Vec<u8> = changed["Value"].try_clone().unwrap().try_into().unwrap();
        message.extend(chunk);
    }
    received.push(String::from_utf8(message[4..].to_vec()).unwrap());
    check("notifications.txt", &(received.join("\n") + "\n"));

    // A phone that connects and then drops: bled advertises again.
    write(&probe, &bled, &[0, 0, 0, 9], 185).await;
    // The adapter's alias is read with Get or GetAll.
    wait_for(&log, "org.freedesktop.DBus.Properties.Get", 2).await;
    let changed: HashMap<&str, Variant<'_>> = HashMap::from([("Connected", Variant::from(false))]);
    let emitter = SignalEmitter::new(&bluez, PHONE).unwrap();
    emitter
        .emit(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", changed, Vec::<String>::new()),
        )
        .await
        .unwrap();
    wait_for(
        &log,
        "org.bluez.LEAdvertisingManager1.RegisterAdvertisement",
        3,
    )
    .await;

    // bluetoothd leaves the bus: bled stops.
    bluez.release_name("org.bluez").await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("bled stops")
        .unwrap();
    let err = ended.expect_err("bled fails when BlueZ leaves");
    push(&log, format!("run: {err}"));

    check("calls.txt", &(log.lock().unwrap().join("\n") + "\n"));
}
