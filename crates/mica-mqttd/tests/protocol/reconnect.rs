//! Reconnecting and bounded calls.

use async_trait::async_trait;
use mica_mqttd::config::Mode;
use mica_mqttd::item::Item;
use mica_mqttd::runtime::ReconnectBackoff;
use mica_mqttd::source::{ItemSource, WriteOutcome};
use mica_mqttd::topic;
use rumqttc::{AsyncClient, MqttOptions};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

/// The schedule itself: doubling from the floor, capped, and reset by any
/// successful poll.
#[test]
pub(super) fn reconnect_backoff_doubles_to_a_ceiling_and_resets() {
    let mut backoff = ReconnectBackoff::default();
    let climb: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
    assert_eq!(
        climb,
        vec![1, 2, 4, 8, 16, 30, 30, 30],
        "the delay must double from 1s and then hold at the 30s ceiling; a backoff that \
         keeps doubling eventually stops retrying a broker that is merely slow to come back"
    );

    backoff.reset();
    assert_eq!(
        backoff.next_delay(),
        Duration::from_secs(1),
        "a connection that worked must not inherit the penalty of the outage before it"
    );
}

/// The FIRST delay is what stops the spin, so it is asserted on its own: a
/// backoff whose floor drifted to zero is a backoff that does nothing, and
/// every other assertion in the test above still passes.
#[test]
pub(super) fn the_first_reconnect_delay_is_not_zero() {
    let first = ReconnectBackoff::default().next_delay();
    assert!(
        first >= Duration::from_millis(500),
        "the first retry delay is {first:?}; a refused connection returns in microseconds, so \
         anything near zero is still a spin loop"
    );
}

/// The PREMISE the backoff exists for, held as a test against the real
/// upstream event loop.
#[tokio::test]
pub(super) async fn rumqttc_reconnects_with_no_delay_of_its_own() {
    let mut options = MqttOptions::new("mica-mqttd-backoff-premise", "127.0.0.1", 1);
    options.set_keep_alive(Duration::from_secs(30));
    let (_client, mut eventloop) = AsyncClient::new(options, 8);

    let started = std::time::Instant::now();
    let mut refusals = 0;
    for _ in 0..5 {
        if eventloop.poll().await.is_err() {
            refusals += 1;
        }
    }
    let elapsed = started.elapsed();

    assert_eq!(
        refusals, 5,
        "nothing listens on 127.0.0.1:1, so every poll must fail; if they succeeded this \
         test is measuring something other than a refused connection"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "five refused reconnects took {elapsed:?}. rumqttc now delays between attempts, so \
         runtime.rs's ReconnectBackoff is stacked on top of an upstream backoff and should \
         be reconsidered -- see the RECONNECT_BACKOFF_MIN docs"
    );
}

/// An application that accepts a call and never answers must not hold the
/// bridge: the bound is applied above the bus, so a hung `GetItems` is an
/// error and a hung `SetValue` is an unreachable write, both within the
/// configured window.
pub(super) struct Hung;

#[async_trait]
impl ItemSource for Hung {
    async fn get_items(
        &self,
        _application: &topic::Application,
    ) -> anyhow::Result<BTreeMap<String, Item>> {
        std::future::pending().await
    }

    async fn set_value(
        &self,
        _application: &topic::Application,
        _path: &str,
        _value: Json,
    ) -> WriteOutcome {
        std::future::pending().await
    }
}

#[tokio::test]
pub(super) async fn a_hung_application_is_bounded_by_the_call_timeout() {
    let source = mica_mqttd::source::Bounded::new(Hung, Duration::from_millis(50));
    let application = application();

    let started = std::time::Instant::now();
    let items = source.get_items(&application).await;
    let outcome = source
        .set_value(&application, "/Enabled", json!(true))
        .await;
    let elapsed = started.elapsed();

    assert!(
        items.is_err(),
        "a hung GetItems must be an error, not a wait"
    );
    assert!(
        matches!(outcome, WriteOutcome::Unreachable { .. }),
        "a hung SetValue is an unreachable write: {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "both calls together took {elapsed:?}; the bound is not being applied"
    );
}

/// A read of a path no application publishes is not an invitation to create
/// a retained topic under the client's chosen name. Nothing is published,
/// and the bridge remembers nothing about the path.
#[tokio::test]
pub(super) async fn a_read_of_an_unknown_path_publishes_nothing() {
    let mut harness = Harness::new(Mode::ReadOnly);
    harness.start(secs(0)).await;
    harness.transport.clear();

    let effects = harness.bridge.on_request(
        secs(1),
        &format!("R/{DEVICE}/{CLASS}/0/NoSuchItem/at/all"),
        b"",
    );
    harness.run(effects).await;

    assert!(
        harness.transport.topics().is_empty(),
        "an unknown path must not become a retained topic: {:?}",
        harness.transport.topics()
    );
}
