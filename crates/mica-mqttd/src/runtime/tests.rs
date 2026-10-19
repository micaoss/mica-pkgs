use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;

use super::{
    ActiveApplication, ApplicationEvent, Incoming, forward, handle_application_event,
    mica_owner_rule,
};
use crate::bridge::Bridge;
use crate::config::{Mode, Timings};
use crate::item::Item;

#[test]
fn the_bus_filters_discovery_to_the_mica_namespace() {
    let rule = mica_owner_rule().expect("valid mica ownership rule");
    let rendered = rule.to_string();
    assert!(rendered.contains("arg0namespace='com.mica'"), "{rendered}");
    assert!(rendered.contains("member='NameOwnerChanged'"), "{rendered}");
}

#[tokio::test]
async fn a_stopped_current_watcher_withdraws_stale_application_state() {
    let enrollment = crate::enrollment::Enrollment::from_names(["com.mica.sensor.example"])
        .expect("valid enrollment");
    let application = enrollment
        .application("com.mica.sensor.example")
        .expect("enrolled application");
    let mut bridge = Bridge::new("abc123", Mode::Full, Timings::default());
    bridge.upsert_service(
        Duration::ZERO,
        application,
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(0))),
            ("/Temperature".to_string(), Item::new(json!(21))),
        ]),
    );
    bridge.on_keepalive(Duration::ZERO);

    let mut active = BTreeMap::from([(
        "com.mica.sensor.example".to_string(),
        ActiveApplication {
            owner: ":1.42".to_string(),
            generation: 7,
            watcher: tokio::spawn(std::future::pending()),
        },
    )]);
    let (effects, resweep) = handle_application_event(
        Duration::from_secs(1),
        &mut bridge,
        &mut active,
        ApplicationEvent::WatcherStopped {
            bus_name: "com.mica.sensor.example".to_string(),
            generation: 7,
            detail: "signal stream ended".to_string(),
        },
    );

    assert!(active.is_empty(), "the stale mirror remained active");
    assert!(
        resweep,
        "the owner is still on the bus, so the runtime must sweep again rather than wait for a restart"
    );
    assert!(
        effects.publications.iter().any(|publication| {
            publication.topic == "N/abc123/sensor/0/Temperature"
                && publication.payload.is_empty()
                && publication.retain
        }),
        "the stale retained value was not withdrawn"
    );
}

#[tokio::test]
async fn a_stale_watcher_report_asks_for_no_resweep() {
    let mut bridge = Bridge::new("abc123", Mode::Full, Timings::default());
    let mut active = BTreeMap::from([(
        "com.mica.sensor.example".to_string(),
        ActiveApplication {
            owner: ":1.42".to_string(),
            generation: 8,
            watcher: tokio::spawn(std::future::pending()),
        },
    )]);
    let (effects, resweep) = handle_application_event(
        Duration::from_secs(1),
        &mut bridge,
        &mut active,
        ApplicationEvent::WatcherStopped {
            bus_name: "com.mica.sensor.example".to_string(),
            generation: 7,
            detail: "signal stream ended".to_string(),
        },
    );
    assert_eq!(effects, crate::bridge::Effects::default());
    assert!(
        !resweep,
        "a report from a superseded watcher changes nothing"
    );
    assert_eq!(active.len(), 1);
}

/// The event-loop task must never wait on the runtime. A request that
/// arrives while the inbound channel is full is dropped, not queued
/// behind a publish that is itself waiting on the event loop.
#[test]
fn an_inbound_request_is_dropped_rather_than_awaited_when_the_runtime_is_busy() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let message = || Incoming {
        topic: "R/abc123/keepalive".to_string(),
        payload: Vec::new(),
    };
    assert!(forward(&tx, message()));
    assert!(forward(&tx, message()));
    assert!(rx.try_recv().is_ok(), "the first request is queued");
    assert!(
        rx.try_recv().is_err(),
        "the second request was dropped, not awaited"
    );
}
