//! [`BusSource`] and the `ItemsChanged` proxy against an application on a
//! private dbus-daemon: what the bridge reads, what each `SetValue` carries on
//! the wire, and how every reply maps to a [`WriteOutcome`].

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::json;
use zbus::object_server::SignalEmitter;
use zbus::{OwnedVariant, Variant};

use super::{BusSource, ItemSource, ItemTreeProxy, WriteOutcome, batch_of, json_of};
use crate::topic::Application;

/// A dbus-daemon of the session configuration on a socket of its own.
struct Bus {
    daemon: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl Bus {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("bus");
        let address = format!("unix:path={}", socket.display());
        let daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(format!("--address={address}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("dbus-daemon runs");
        wait_for(&socket);
        Self {
            daemon,
            address,
            _dir: dir,
        }
    }

    async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .build()
            .await
            .expect("connects to the private bus")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn wait_for(socket: &Path) {
    for _ in 0..500 {
        if socket.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("dbus-daemon did not listen on {}", socket.display());
}

fn attrs(entries: &[(&str, Variant<'_>)]) -> HashMap<String, OwnedVariant> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.try_to_owned().expect("owned")))
        .collect()
}

fn tree() -> HashMap<String, HashMap<String, OwnedVariant>> {
    HashMap::from([
        (
            "/Calibration/gain".to_string(),
            attrs(&[
                ("value", Variant::from(1.5)),
                ("writable", Variant::from(true)),
                ("min", Variant::from(0.0)),
                ("max", Variant::from(10.0)),
            ]),
        ),
        (
            "/Status/name".to_string(),
            attrs(&[("value", Variant::from("pump"))]),
        ),
        (
            "/Status/counts".to_string(),
            attrs(&[
                ("value", Variant::from(vec![1u32, 2])),
                ("writable", Variant::from(false)),
            ]),
        ),
        (
            "/Status/flags".to_string(),
            attrs(&[(
                "value",
                Variant::from(HashMap::from([("on".to_string(), Variant::from(true))])),
            )]),
        ),
        (
            "/Status/path".to_string(),
            attrs(&[(
                "value",
                Variant::from(zbus::ObjectPath::from_static_str_unchecked("/a/b")),
            )]),
        ),
        (
            "/Empty".to_string(),
            attrs(&[("value", Variant::from(Vec::<u8>::new()))]),
        ),
        (
            "/NoValue".to_string(),
            attrs(&[("writable", Variant::from(true))]),
        ),
    ])
}

/// `/`: the tree half, with no `SetValue`.
struct Tree;

#[zbus::interface(name = "com.mica.Item1")]
impl Tree {
    fn get_items(&self) -> HashMap<String, HashMap<String, OwnedVariant>> {
        tree()
    }

    #[zbus(signal)]
    async fn items_changed(
        emitter: &SignalEmitter<'_>,
        items: HashMap<String, HashMap<String, OwnedVariant>>,
    ) -> zbus::Result<()>;
}

/// One item: records the signature and value of each `SetValue` and answers
/// with its code.
struct Leaf {
    code: i32,
    seen: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "com.mica.Item1")]
impl Leaf {
    fn set_value(&self, value: OwnedVariant) -> i32 {
        self.seen.lock().expect("lock").push(format!(
            "{} {}",
            value.value_signature(),
            json_of(&value)
        ));
        self.code
    }
}

/// An object that is not an item.
struct Other;

#[zbus::interface(name = "com.mica.Other1")]
impl Other {
    fn ping(&self) {}
}

#[tokio::test]
async fn the_bridge_reads_writes_and_watches_an_application_on_the_bus() {
    let bus = Bus::start();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = bus.connect().await;
    let server = app.object_server();
    server.at("/", Tree).await.expect("tree");
    for (path, code) in [("/Calibration/gain", 0), ("/Calibration/limit", 3)] {
        server
            .at(
                path,
                Leaf {
                    code,
                    seen: seen.clone(),
                },
            )
            .await
            .expect("leaf");
    }
    server.at("/Other", Other).await.expect("other");
    app.request_name("com.mica.probe").await.expect("name");

    let bridge = bus.connect().await;
    let source = BusSource::new(bridge.clone());
    let probe = Application::from_enrollment("com.mica.probe").expect("enrollment");

    let items = source.get_items(&probe).await.expect("GetItems");
    assert_eq!(
        format!("{items:?}"),
        concat!(
            r#"{"/Calibration/gain": Item { value: Number(1.5), writable: true, min: Some(Number(0.0)), max: Some(Number(10.0)) }, "#,
            r#""/Status/counts": Item { value: Array [Number(1), Number(2)], writable: false, min: None, max: None }, "#,
            r#""/Status/flags": Item { value: Object {"on": Bool(true)}, writable: false, min: None, max: None }, "#,
            r#""/Status/name": Item { value: String("pump"), writable: false, min: None, max: None }, "#,
            r#""/Status/path": Item { value: String("/a/b"), writable: false, min: None, max: None }}"#,
        )
    );

    let writes = [
        ("/Calibration/gain", json!(1.5), WriteOutcome::Accepted),
        ("/Calibration/gain", json!(7), WriteOutcome::Accepted),
        ("/Calibration/gain", json!(u64::MAX), WriteOutcome::Accepted),
        ("/Calibration/gain", json!("on"), WriteOutcome::Accepted),
        ("/Calibration/gain", json!(true), WriteOutcome::Accepted),
        (
            "/Calibration/gain",
            json!([1, "a", null]),
            WriteOutcome::Accepted,
        ),
        (
            "/Calibration/gain",
            json!({"k": {"n": 2}, "z": null}),
            WriteOutcome::Accepted,
        ),
        (
            "/Calibration/limit",
            json!(7),
            WriteOutcome::Refused { code: 3 },
        ),
        (
            "/Calibration/missing",
            json!(1),
            WriteOutcome::UnknownObject,
        ),
        ("/Other", json!(1), WriteOutcome::UnknownObject),
        ("/", json!(1), WriteOutcome::UnknownObject),
        ("not a path", json!(1), WriteOutcome::UnknownObject),
        (
            "/Calibration/gain",
            json!(null),
            WriteOutcome::Unrepresentable,
        ),
    ];
    for (path, value, want) in writes {
        let got = source.set_value(&probe, path, value.clone()).await;
        assert_eq!(got, want, "SetValue {value} at {path}");
    }
    assert_eq!(
        *seen.lock().expect("lock"),
        [
            "d 1.5",
            "x 7",
            "t 18446744073709551615",
            "s \"on\"",
            "b true",
            "av [1,\"a\"]",
            "a{sv} {\"k\":{\"n\":2}}",
            "x 7",
        ]
    );

    let absent = Application::from_enrollment("com.mica.absent").expect("enrollment");
    assert!(
        matches!(
            source
                .set_value(&absent, "/Calibration/gain", json!(1))
                .await,
            WriteOutcome::Unreachable { .. }
        ),
        "a service that is not on the bus is unreachable"
    );
    assert!(source.get_items(&absent).await.is_err());

    let proxy = ItemTreeProxy::builder(&bridge)
        .destination("com.mica.probe")
        .build()
        .await
        .expect("proxy");
    let mut changes = proxy.receive_items_changed().await.expect("subscribe");
    let emitter = SignalEmitter::new(&app, "/").expect("emitter");
    let mut changed = tree();
    changed.retain(|path, _| path == "/Calibration/gain" || path == "/NoValue");
    Tree::items_changed(&emitter, changed).await.expect("emit");
    let signal = tokio::time::timeout(Duration::from_secs(5), changes.next())
        .await
        .expect("ItemsChanged arrives")
        .expect("stream open");
    let batch: BTreeMap<_, _> = batch_of(signal.args().expect("args").items);
    assert_eq!(
        format!("{batch:?}"),
        concat!(
            r#"{"/Calibration/gain": Some(Item { value: Number(1.5), writable: true, min: Some(Number(0.0)), max: Some(Number(10.0)) }), "#,
            r#""/NoValue": None}"#,
        )
    );
}
