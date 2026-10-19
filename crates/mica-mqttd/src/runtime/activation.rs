//! Activating one application: its subscription, its first publish and its watch.

use crate::bridge::{Bridge, Effects};
use crate::enrollment::Enrollment;
use crate::source::ItemSource;
use crate::topic::Application;
use crate::transport::Transport;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use super::*;

/// The mirrors the runtime holds, and what it needs to open one.
pub(super) struct Activation<'a> {
    pub(super) connection: &'a zbus::Connection,
    pub(super) source: &'a dyn ItemSource,
    pub(super) changes_tx: &'a mpsc::Sender<ApplicationEvent>,
    pub(super) active: BTreeMap<String, ActiveApplication>,
    pub(super) next_generation: u64,
}

impl Activation<'_> {
    pub(super) async fn activate(
        &mut self,
        bridge: &mut Bridge,
        now: Duration,
        application: Application,
        owner: String,
    ) -> Effects {
        let bus_name = application.bus_name().to_string();
        if self
            .active
            .get(&bus_name)
            .is_some_and(|current| current.owner == owner)
        {
            return Effects::default();
        }

        let effects = if self.active.remove(&bus_name).is_some() {
            bridge.on_service_vanished(now, &bus_name)
        } else {
            Effects::default()
        };

        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let watcher_application = application.clone();
        let watcher_connection = self.connection.clone();
        let watcher_tx = self.changes_tx.clone();
        let stopped_tx = self.changes_tx.clone();
        let stopped_application = application.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let watcher = tokio::spawn(async move {
            if let Err(err) = watch_application(
                watcher_connection,
                watcher_application.clone(),
                generation,
                ready_tx,
                watcher_tx,
            )
            .await
            {
                let _ = stopped_tx
                    .send(ApplicationEvent::WatcherStopped {
                        bus_name: stopped_application.bus_name().to_string(),
                        generation,
                        detail: err.to_string(),
                    })
                    .await;
            }
        });

        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(detail)) => {
                watcher.abort();
                tracing::warn!(
                    application = application.bus_name(),
                    error = detail,
                    "application is present but its ItemsChanged watcher cannot be established"
                );
                return effects;
            }
            Err(_) => {
                watcher.abort();
                tracing::warn!(
                    application = application.bus_name(),
                    "application ItemsChanged watcher stopped before it became ready"
                );
                return effects;
            }
        }

        match self.source.get_items(&application).await {
            Ok(items) => {
                self.active.insert(
                    bus_name,
                    ActiveApplication {
                        owner,
                        generation,
                        watcher,
                    },
                );
                merge(effects, bridge.upsert_service(now, application, items))
            }
            Err(err) => {
                watcher.abort();
                tracing::warn!(
                    application = application.bus_name(),
                    error = %err,
                    "application is present but its Item1 tree is not readable; check its exact-name D-Bus policy grant"
                );
                effects
            }
        }
    }

    /// Activate every enrolled name that is on the bus and not yet mirrored.
    /// Returns whether any of them could not be activated, so the caller can
    /// come back after [`ACTIVATION_RETRY`].
    pub(super) async fn sweep(
        &mut self,
        bus: &zbus::fdo::DBusProxy<'_>,
        enrollment: &Enrollment,
        bridge: &mut Bridge,
        transport: &dyn Transport,
        start: Instant,
    ) -> anyhow::Result<bool> {
        let mut failed = false;
        for name in bus.list_names().await? {
            let Some(application) = enrollment.application(name.as_str()) else {
                continue;
            };
            let owner = match bus.get_name_owner(name.clone().into()).await {
                Ok(owner) => owner.to_string(),
                Err(_) => continue,
            };
            let effects = self
                .activate(bridge, start.elapsed(), application, owner)
                .await;
            apply(effects, transport, self.source).await?;
            failed |= !self.active.contains_key(name.as_str());
        }
        Ok(failed)
    }
}
