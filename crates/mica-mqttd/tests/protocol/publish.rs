//! Publishing: verbs, keepalive, masking, read-only mode and vanished applications.

use mica_mqttd::bridge::Effects;
use mica_mqttd::config::Mode;
use mica_mqttd::item::Item;
use mica_mqttd::source::{ItemSource, WriteOutcome};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

/// (a) N, R and W each map to what the grammar says they map to.
#[tokio::test]
pub(super) async fn verbs_map_to_notify_read_and_write() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;

    // N: the full republish carries every item under its own topic.
    assert_eq!(
        harness.transport.payload(&notify("/Temperature")),
        json!({"value": 21})
    );
    assert_eq!(
        harness.transport.payload(&notify("/Enabled")),
        json!({"value": true})
    );
    assert_eq!(
        harness.transport.payload(&notify("/Calibration/offset")),
        json!({"value": 0})
    );
    assert_eq!(
        harness
            .transport
            .payload(&format!("N/{DEVICE}/full_publish_completed")),
        json!({"value": 5}),
        "the marker reports how many items preceded it"
    );

    // N: one item change publishes exactly that item, and nothing else.
    harness.transport.clear();
    let effects = harness.bridge.on_items_changed(
        secs(1),
        APPLICATION_SERVICE,
        BTreeMap::from([("/Temperature".to_string(), Some(Item::new(json!(22))))]),
    );
    harness.run(effects).await;
    assert_eq!(harness.transport.topics(), vec![notify("/Temperature")]);
    assert_eq!(
        harness.transport.payload(&notify("/Temperature")),
        json!({"value": 22})
    );
    assert!(
        harness.transport.published.lock().unwrap()[0].retain,
        "item notifications are retained so a late subscriber still finds the tree"
    );

    // R: the addressed item is republished on its N topic.
    harness.transport.clear();
    let effects =
        harness
            .bridge
            .on_request(secs(2), &format!("R/{DEVICE}/{CLASS}/0/SampleCount"), b"");
    harness.run(effects).await;
    assert_eq!(harness.transport.topics(), vec![notify("/SampleCount")]);
    assert_eq!(
        harness.transport.payload(&notify("/SampleCount")),
        json!({"value": 42})
    );

    // W: the addressed item is written, and nothing is published for it.
    harness.transport.clear();
    let effects = harness.bridge.on_request(
        secs(3),
        &format!("W/{DEVICE}/{CLASS}/0/Enabled"),
        br#"{"value": false}"#,
    );
    harness.run(effects).await;
    assert_eq!(
        harness.source.writes(),
        vec![(
            APPLICATION_SERVICE.to_string(),
            "/Enabled".to_string(),
            json!(false)
        )]
    );
    assert!(harness.transport.topics().is_empty());

    // An item that became invalid publishes the JSON form of the bus sentinel,
    // so a subscriber sees the transition rather than inferring it.
    harness.transport.clear();
    let effects = harness.bridge.on_items_changed(
        secs(4),
        APPLICATION_SERVICE,
        BTreeMap::from([("/SampleCount".to_string(), None)]),
    );
    harness.run(effects).await;
    assert_eq!(
        harness.transport.payload(&notify("/SampleCount")),
        json!({ "value": Json::Null })
    );

    // Topics belonging to someone else are not instructions to this bridge.
    harness.transport.clear();
    for foreign in [
        format!("R/other-device/{CLASS}/0/Temperature"),
        format!("R/{DEVICE}/meter/0/Temperature"),
        format!("R/{DEVICE}/{CLASS}/7/Temperature"),
        notify("/Temperature"),
    ] {
        let effects = harness.bridge.on_request(secs(5), &foreign, b"");
        assert_eq!(effects, Effects::default(), "{foreign} was acted on");
    }
}

/// (b) A keepalive triggers a full republish, and a storm of them does not
/// multiply it.
#[tokio::test]
pub(super) async fn keepalive_republishes_fully_and_is_rate_limited() {
    let mut harness = Harness::new(Mode::Full);
    let marker = format!("N/{DEVICE}/full_publish_completed");

    // Nothing at all before the first keepalive: publishing is gated on the
    // alive window.
    let application = application();
    let items = harness.source.get_items(&application).await.expect("seed");
    let effects = harness.bridge.upsert_service(secs(0), application, items);
    harness.run(effects).await;
    assert!(
        harness.transport.topics().is_empty(),
        "the bridge published before any keepalive armed it"
    );

    // Ten keepalives inside the rate limit's floor.
    for tenth in 0..10 {
        let now = Duration::from_millis(tenth * 100);
        let effects = harness
            .bridge
            .on_request(now, &format!("R/{DEVICE}/keepalive"), b"");
        harness.run(effects).await;
    }
    assert_eq!(
        harness.transport.count(&marker),
        1,
        "a keepalive storm produced more than one full republish"
    );
    assert_eq!(
        harness.transport.count(&notify("/Temperature")),
        1,
        "the storm republished the tree more than once"
    );

    // Every one of them renewed the window, which is what must not be
    // throttled: the last keepalive landed at 0.9 s, so the window runs to
    // 60.9 s.
    assert!(harness.bridge.alive(secs(60)));
    assert!(!harness.bridge.alive(secs(61)));

    // The republishes the floor swallowed coalesce into exactly one, once the
    // floor has passed.
    let effects = harness.bridge.on_tick(secs(5));
    harness.run(effects).await;
    assert_eq!(
        harness.transport.count(&marker),
        2,
        "the deferred republish did not coalesce into exactly one"
    );

    // The heartbeat runs at 3 s while alive: the tick above beat at 5 s, so
    // the next is due at 8 s and the one after at 11 s, and the ticks in
    // between produce nothing.
    harness.transport.clear();
    let beat = format!("N/{DEVICE}/heartbeat");
    for (tick, expected) in [
        (secs(6), 0),
        (secs(8), 1),
        (secs(9), 1),
        (secs(10), 1),
        (secs(11), 2),
    ] {
        let effects = harness.bridge.on_tick(tick);
        harness.run(effects).await;
        assert_eq!(
            harness.transport.count(&beat),
            expected,
            "heartbeat cadence is wrong at {tick:?}"
        );
    }
    assert_eq!(
        harness.transport.last_payload(&beat),
        json!({"value": 11}),
        "the beat carries the bridge's uptime in seconds"
    );
    harness.transport.clear();
    let effects = harness.bridge.on_tick(secs(120));
    harness.run(effects).await;
    assert!(
        harness.transport.topics().is_empty(),
        "the bridge kept publishing after the alive window expired"
    );
}

/// (c) Secrets are masked at publish, structurally, at any depth.
#[tokio::test]
pub(super) async fn secrets_are_masked_at_publish() {
    const SECRET: &str = "s3cr3t-never-on-the-wire";

    let mut harness = Harness::new(Mode::Full);
    // Applications are responsible for their own source-side redaction; the
    // bridge's structural masking is an independent last line of defence.
    harness
        .source
        .items
        .insert("/Credentials/hash".to_string(), Item::new(json!(SECRET)));
    harness.source.items.insert(
        "/Networks".to_string(),
        Item::new(json!([
            {"ssid": "home", "psk": SECRET},
            {"ssid": "field", "password_hash": SECRET, "nested": {"passwordHash": SECRET}},
        ])),
    );
    harness.start(secs(0)).await;

    // A secret-named path is the secret, so it publishes as invalid rather
    // than as a masked value.
    assert_eq!(
        harness.transport.payload(&notify("/Credentials/hash")),
        json!({ "value": Json::Null })
    );
    // A secret-named key inside a value is stripped, and its siblings are not.
    assert_eq!(
        harness.transport.payload(&notify("/Networks")),
        json!({"value": [
            {"ssid": "home"},
            {"ssid": "field", "nested": {}},
        ]})
    );
    // And, the assertion that does not depend on knowing where to look: the
    // secret is in no payload this bridge produced, anywhere.
    for publication in harness.transport.published.lock().unwrap().iter() {
        let payload = String::from_utf8_lossy(&publication.payload);
        assert!(
            !payload.contains(SECRET),
            "{} carried the secret: {payload}",
            publication.topic
        );
    }
}

/// (d) Read-only mode refuses W, and full mode does not — the same request,
/// so the refusal cannot be an accident of the fixture.
#[tokio::test]
pub(super) async fn read_only_mode_refuses_writes() {
    let topic = format!("W/{DEVICE}/{CLASS}/0/Enabled");
    let payload = br#"{"value": false}"#;

    let mut read_only = Harness::new(Mode::ReadOnly);
    read_only.start(secs(0)).await;
    read_only.transport.clear();
    let effects = read_only.bridge.on_request(secs(1), &topic, payload);
    read_only.run(effects).await;
    assert!(
        read_only.source.writes().is_empty(),
        "a read-only bridge carried a write through to SetValue"
    );
    assert!(read_only.transport.topics().is_empty());
    assert_eq!(
        read_only.bridge.subscriptions(),
        vec![format!("R/{DEVICE}/#")],
        "a read-only bridge subscribed to W topics"
    );

    let mut full = Harness::new(Mode::Full);
    full.start(secs(0)).await;
    let effects = full.bridge.on_request(secs(1), &topic, payload);
    full.run(effects).await;
    assert_eq!(
        full.source.writes(),
        vec![(
            APPLICATION_SERVICE.to_string(),
            "/Enabled".to_string(),
            json!(false)
        )],
        "the same request must reach the bus in full mode"
    );
    assert_eq!(
        full.bridge.subscriptions(),
        vec![format!("R/{DEVICE}/#"), format!("W/{DEVICE}/#")]
    );
}

/// A refused write puts nothing on the wire.
///
/// `SetValue` result codes are deliberately not part of the MQTT grammar. A
/// client observes a successful write through the application's subsequent
/// `ItemsChanged`; a refusal is the absence of that update.
#[tokio::test]
pub(super) async fn a_refused_write_publishes_nothing() {
    for (path, outcome) in [
        ("/Enabled", WriteOutcome::Refused { code: -2 }),
        ("/Calibration/offset", WriteOutcome::UnknownObject),
    ] {
        let mut harness = Harness::new(Mode::Full);
        harness.source.outcome = outcome;
        harness.start(secs(0)).await;
        harness.transport.clear();

        let effects = harness.bridge.on_request(
            secs(1),
            &format!("W/{DEVICE}/{CLASS}/0{path}"),
            br#"{"value": 1}"#,
        );
        harness.run(effects).await;

        assert_eq!(
            harness.source.writes(),
            vec![(APPLICATION_SERVICE.to_string(), path.to_string(), json!(1))],
            "the request must still reach SetValue"
        );
        assert!(
            harness.transport.topics().is_empty(),
            "a refused write on {path} was reported on the wire"
        );
    }
}

/// (e) An application that leaves the bus has its retained state cleared.
#[tokio::test]
pub(super) async fn a_vanished_application_clears_its_retained_state() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;
    let published: Vec<String> = harness
        .transport
        .topics()
        .into_iter()
        .filter(|topic| topic.contains(CLASS))
        .collect();
    assert_eq!(published.len(), 5, "the fixture tree published five items");

    harness.transport.clear();
    let effects = harness
        .bridge
        .on_service_vanished(secs(1), APPLICATION_SERVICE);
    harness.run(effects).await;

    let mut cleared = harness.transport.topics();
    cleared.sort();
    let mut expected = published;
    expected.sort();
    assert_eq!(
        cleared, expected,
        "the clear did not cover every item topic"
    );
    for publication in harness.transport.published.lock().unwrap().iter() {
        assert!(
            publication.payload.is_empty(),
            "{} was cleared with a payload, which sets state instead of deleting it",
            publication.topic
        );
        assert!(
            publication.retain,
            "{} was cleared without the retain flag, so the retained message survives",
            publication.topic
        );
    }
}

/// An application that vanishes while the bridge is silent still owes clears,
/// and pays them at the next keepalive.
///
/// The alive gate makes the vanish itself silent, and the vanish takes the
/// so the pending deletes must survive until the next keepalive.
#[tokio::test]
pub(super) async fn clears_owed_while_silent_are_paid_at_the_next_keepalive() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;
    let published: Vec<String> = harness
        .transport
        .topics()
        .into_iter()
        .filter(|topic| topic.contains(CLASS))
        .collect();

    // The window shuts, and only then does the device leave the bus.
    harness.transport.clear();
    let effects = harness
        .bridge
        .on_service_vanished(secs(120), APPLICATION_SERVICE);
    harness.run(effects).await;
    assert!(
        harness.transport.topics().is_empty(),
        "the bridge published after the alive window expired"
    );

    // The keepalive still parses — the device id outlives the process that
    // reported it — so the window reopens and the owed clears go out.
    let effects = harness
        .bridge
        .on_request(secs(121), &format!("R/{DEVICE}/keepalive"), b"");
    harness.run(effects).await;
    let mut cleared: Vec<String> = harness
        .transport
        .topics()
        .into_iter()
        .filter(|topic| topic.contains(CLASS))
        .collect();
    cleared.sort();
    let mut expected = published;
    expected.sort();
    assert_eq!(cleared, expected, "the owed clears were never paid");
    assert_eq!(
        harness
            .transport
            .payload(&format!("N/{DEVICE}/full_publish_completed")),
        json!({"value": 0}),
        "the republish after the application vanished carries no items"
    );
}
