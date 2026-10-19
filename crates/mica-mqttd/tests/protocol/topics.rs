//! Topics: classes, paths, identities and collisions.

use mica_mqttd::bridge::Bridge;
use mica_mqttd::config::{Mode, Timings};
use mica_mqttd::item::Item;
use mica_mqttd::topic::{self, Address, Request};
use serde_json::json;
use std::collections::BTreeMap;

use super::*;

/// A direct service publishes under the third component of its exact name.
#[tokio::test]
pub(super) async fn a_direct_service_publishes_under_its_class() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;

    let published = harness.transport.topics();
    assert!(
        published.contains(&format!("N/{DEVICE}/{APPLICATION_CLASS}/0/Temperature")),
        "{APPLICATION_SERVICE} did not publish under its class {APPLICATION_CLASS}; saw {published:?}"
    );
}

#[test]
pub(super) fn invalid_application_paths_never_reach_mqtt_topics() {
    let mut bridge = Bridge::new(DEVICE, Mode::Full, Timings::default());
    bridge.upsert_service(
        secs(0),
        application(),
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(0))),
            ("relative".to_string(), Item::new(json!(1))),
            ("/wild/#".to_string(), Item::new(json!(2))),
        ]),
    );
    let topics: Vec<String> = bridge
        .on_keepalive(secs(0))
        .publications
        .into_iter()
        .map(|publication| publication.topic)
        .collect();

    assert!(topics.contains(&"N/abc123/sensor/0/DeviceInstance".to_string()));
    assert!(
        topics
            .iter()
            .all(|topic| !topic.contains("relative") && !topic.contains('#')),
        "an application-controlled invalid object path reached MQTT: {topics:?}"
    );
    assert!(
        bridge
            .on_items_changed(
                secs(1),
                APPLICATION_SERVICE,
                BTreeMap::from([("/bad/+".to_string(), Some(Item::new(json!(3))))]),
            )
            .publications
            .is_empty(),
        "an invalid ItemsChanged key reached MQTT"
    );
}

#[test]
pub(super) fn mqtt_topic_identity_and_item_path_inputs_are_strict() {
    for segment in [
        "",
        "device/other",
        "device+",
        "device#",
        "device other",
        "device$other",
        "设备",
        "device\0other",
        "device\nother",
    ] {
        assert!(
            !topic::valid_topic_segment(segment),
            "invalid MQTT segment was accepted: {segment:?}"
        );
    }
    assert!(topic::valid_topic_segment(DEVICE));

    for request in [
        format!("R/{DEVICE}/{CLASS}/0/not-an-object-path"),
        format!("W/{DEVICE}/{CLASS}/0/bad-path"),
    ] {
        assert_eq!(
            topic::parse(&request, DEVICE),
            None,
            "invalid D-Bus path was accepted: {request}"
        );
    }
}

#[test]
pub(super) fn multiple_applications_share_one_device_liveness_protocol() {
    let sensor = application();
    let meter = application_named("com.mica.meter.abc123");
    let mut bridge = Bridge::new(DEVICE, Mode::Full, Timings::default());
    bridge.upsert_service(
        secs(0),
        sensor,
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(0))),
            ("/Temperature".to_string(), Item::new(json!(21))),
        ]),
    );
    bridge.upsert_service(
        secs(0),
        meter,
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(2))),
            ("/Power".to_string(), Item::new(json!(900))),
        ]),
    );

    let full = bridge.on_keepalive(secs(0));
    let topics: Vec<&str> = full
        .publications
        .iter()
        .map(|publication| publication.topic.as_str())
        .collect();
    assert!(topics.contains(&"N/abc123/sensor/0/Temperature"));
    assert!(topics.contains(&"N/abc123/meter/2/Power"));
    assert_eq!(
        topics
            .iter()
            .filter(|topic| **topic == "N/abc123/full_publish_completed")
            .count(),
        1,
        "a device-wide full publish has one completion marker"
    );
    assert_eq!(
        bridge
            .on_tick(secs(3))
            .publications
            .iter()
            .filter(|publication| publication.topic == "N/abc123/heartbeat")
            .count(),
        1,
        "multiple applications must not duplicate the device heartbeat"
    );
}

#[test]
pub(super) fn a_class_instance_collision_fails_closed_for_publication_and_control() {
    let first = application();
    let second = application_named("com.mica.sensor.second");
    let items = BTreeMap::from([
        ("/DeviceInstance".to_string(), Item::new(json!(0))),
        ("/Enabled".to_string(), Item::writable(json!(true))),
    ]);
    let mut bridge = Bridge::new(DEVICE, Mode::Full, Timings::default());
    bridge.upsert_service(secs(0), first, items.clone());
    bridge.on_keepalive(secs(0));

    let collision = bridge.upsert_service(secs(1), second, items);
    assert!(
        collision
            .publications
            .iter()
            .all(|publication| publication.payload.is_empty()),
        "introducing a collision may clear old retained state but must publish neither claimant"
    );
    assert!(
        bridge
            .on_request(secs(2), "W/abc123/sensor/0/Enabled", br#"{"value": false}"#,)
            .writes
            .is_empty(),
        "an ambiguous write must not reach either application"
    );

    let restored = bridge.on_service_vanished(secs(5), "com.mica.sensor.second");
    assert!(
        restored
            .publications
            .iter()
            .any(|publication| publication.topic == "N/abc123/sensor/0/Enabled"),
        "the remaining application must become publishable when the collision clears"
    );
}

/// Building and parsing agree for a direct application's address.
#[test]
pub(super) fn an_application_topic_round_trips() {
    let address = Address {
        device_id: DEVICE.to_string(),
        class: topic::class_of(APPLICATION_SERVICE)
            .expect("a com.mica.* bus name")
            .to_string(),
        instance: 0,
    };

    let read = address.item_topic(topic::READ, "/SampleCount");
    assert_eq!(
        read,
        format!("R/{DEVICE}/{APPLICATION_CLASS}/0/SampleCount")
    );
    assert_eq!(
        topic::parse(&read, DEVICE),
        Some(Request::Read {
            class: APPLICATION_CLASS.to_string(),
            instance: 0,
            path: "/SampleCount".to_string()
        })
    );

    let under_ext = format!("R/{DEVICE}/ext/0/SampleCount");
    assert_eq!(
        topic::parse(&under_ext, DEVICE),
        Some(Request::Read {
            class: "ext".to_string(),
            instance: 0,
            path: "/SampleCount".to_string()
        }),
        "the grammar parses an address before the application bridge decides whether a service owns it"
    );
}

/// `ext` is an ordinary direct class; only the bare `com.mica` namespace
/// has no class.
#[test]
pub(super) fn direct_name_classification_has_no_extension_special_case() {
    assert_eq!(topic::class_of("com.mica.ext"), Some("ext"));
    assert_eq!(topic::class_of("com.mica.ext.sensor"), Some("ext"));
    assert_eq!(topic::class_of("com.mica"), None);
}

// ---------------------------------------------------------------------------
// Reconnect backoff
// ---------------------------------------------------------------------------
