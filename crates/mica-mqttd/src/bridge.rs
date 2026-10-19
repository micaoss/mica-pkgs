//! Device-wide MQTT protocol state over application item trees.
//!
//! The bridge accepts only typed [`Application`] values produced by package
//! [`Enrollment`](crate::enrollment::Enrollment). That makes the remote
//! publication boundary structural: an arbitrary observed bus name is
//! unrepresentable here rather than a path a growing denylist must remember.
//!
//! One bridge coordinates every application on the device. Keepalive,
//! heartbeat, subscriptions and `full_publish_completed` are device-scoped,
//! so they are emitted once even when several application services are live.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::Value as Json;

use crate::config::{Mode, Timings};
use crate::item::Item;
use crate::payload;
use crate::topic::{self, Address, Application, Request};

/// One MQTT publication the runtime is to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub topic: String,
    pub payload: Vec<u8>,
    /// Item notifications are retained. Heartbeats and completion markers are
    /// momentary and therefore are not retained.
    pub retain: bool,
}

/// A `SetValue` the runtime is to perform on one exact application service.
#[derive(Debug, Clone, PartialEq)]
pub struct Write {
    pub application: Application,
    pub path: String,
    pub value: Json,
}

/// What one event asks the runtime to do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Effects {
    pub publications: Vec<Publication>,
    pub writes: Vec<Write>,
}

impl From<Vec<Publication>> for Effects {
    fn from(publications: Vec<Publication>) -> Self {
        Self {
            publications,
            ..Self::default()
        }
    }
}

struct Service {
    application: Application,
    items: BTreeMap<String, Item>,
    /// Retained topics written for this service, kept in full form so an
    /// instance change or disappearance can delete their broker state.
    published: BTreeSet<String>,
}

impl Service {
    fn address(&self, device_id: &str) -> Address {
        Address {
            device_id: device_id.to_string(),
            class: self.application.class().to_string(),
            instance: topic::instance_of(&self.items),
        }
    }
}

/// The complete device-side protocol state.
pub struct Bridge {
    device_id: String,
    mode: Mode,
    timings: Timings,
    services: BTreeMap<String, Service>,
    /// Retained deletes owed after a service, address or collision changed.
    pending_clears: BTreeSet<String>,
    alive_until: Option<Duration>,
    last_full: Option<Duration>,
    full_pending: bool,
    next_heartbeat: Duration,
}

impl Bridge {
    pub fn new(device_id: impl Into<String>, mode: Mode, timings: Timings) -> Self {
        Self {
            device_id: device_id.into(),
            mode,
            timings,
            services: BTreeMap::new(),
            pending_clears: BTreeSet::new(),
            alive_until: None,
            last_full: None,
            full_pending: false,
            next_heartbeat: Duration::ZERO,
        }
    }

    /// Device identity used in every topic. It comes from the narrow
    /// management method, never from an item tree.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Whether a keepalive window is currently open.
    pub fn alive(&self, now: Duration) -> bool {
        self.alive_until.is_some_and(|until| now < until)
    }

    /// The device-scoped subscription filters. They exist even with no live
    /// application so a keepalive can open the window before an app appears.
    pub fn subscriptions(&self) -> Vec<String> {
        let mut filters = vec![format!("{}/{}/#", topic::READ, self.device_id)];
        if self.mode.writes_allowed() {
            filters.push(format!("{}/{}/#", topic::WRITE, self.device_id));
        }
        filters
    }

    /// Add or replace one application mirror.
    ///
    /// The [`Application`] argument is the admission gate: there is no public
    /// constructor for a system service. Replacing any service can create or
    /// clear a class/instance collision, so all retained topics are cleared
    /// before the unique addresses are republished.
    pub fn upsert_service(
        &mut self,
        now: Duration,
        application: Application,
        items: BTreeMap<String, Item>,
    ) -> Effects {
        self.clear_all_publications();
        let items = items
            .into_iter()
            .filter(|(path, _)| {
                let valid = topic::valid_item_path(path);
                if !valid {
                    tracing::warn!(
                        application = application.bus_name(),
                        path,
                        "application GetItems returned an invalid object path; ignoring it"
                    );
                }
                valid
            })
            .collect();
        self.services.insert(
            application.bus_name().to_string(),
            Service {
                application,
                items,
                published: BTreeSet::new(),
            },
        );
        self.after_topology_change(now).into()
    }

    /// Apply a coalesced `ItemsChanged` batch from one admitted application.
    pub fn on_items_changed(
        &mut self,
        now: Duration,
        bus_name: &str,
        batch: BTreeMap<String, Option<Item>>,
    ) -> Effects {
        let Some(service) = self.services.get_mut(bus_name) else {
            return Effects::default();
        };
        let old_instance = topic::instance_of(&service.items);
        let batch: BTreeMap<_, _> = batch
            .into_iter()
            .filter(|(path, _)| {
                let valid = topic::valid_item_path(path);
                if !valid {
                    tracing::warn!(
                        application = bus_name,
                        path,
                        "application ItemsChanged carried an invalid object path; ignoring it"
                    );
                }
                valid
            })
            .collect();
        for (path, item) in &batch {
            match item {
                Some(item) => service.items.insert(path.clone(), item.clone()),
                None => service.items.remove(path),
            };
        }
        let new_instance = topic::instance_of(&service.items);
        if old_instance != new_instance {
            self.clear_all_publications();
            return self.after_topology_change(now).into();
        }
        if !self.alive(now) {
            return Effects::default();
        }
        batch
            .iter()
            .filter_map(|(path, item)| self.publish_item(bus_name, path, item.as_ref()))
            .collect::<Vec<_>>()
            .into()
    }

    /// Remove an application and clear retained state for its address. All
    /// services are reconsidered because removing one side of a collision
    /// makes the remaining side publishable again.
    pub fn on_service_vanished(&mut self, now: Duration, bus_name: &str) -> Effects {
        if !self.services.contains_key(bus_name) {
            return Effects::default();
        }
        self.clear_all_publications();
        self.services.remove(bus_name);
        self.after_topology_change(now).into()
    }

    /// A keepalive opens or renews the publishing window and requests one
    /// full, device-wide republish.
    pub fn on_keepalive(&mut self, now: Duration) -> Effects {
        if !self.alive(now) {
            self.next_heartbeat = now + self.timings.heartbeat;
        }
        self.alive_until = Some(now + self.timings.alive_window);
        self.request_full(now).into()
    }

    /// One broker message. Reads and writes are routed only when exactly one
    /// admitted application owns the requested class/instance address.
    pub fn on_request(&mut self, now: Duration, topic: &str, body: &[u8]) -> Effects {
        let Some(request) = topic::parse(topic, &self.device_id) else {
            tracing::debug!(topic, "request not addressed to this device");
            return Effects::default();
        };
        match request {
            Request::Keepalive => self.on_keepalive(now),
            Request::Read {
                class,
                instance,
                path,
            } => {
                if !self.alive(now) {
                    return Effects::default();
                }
                let Some(bus_name) = self.unique_service(&class, instance) else {
                    tracing::warn!(class, instance, "read has no unique application target");
                    return Effects::default();
                };
                // A path the application does not publish is not republished
                // as invalid: that would let a broker client mint retained
                // topics under names of its choosing, and grow `published`
                // by one entry per request.
                let Some(item) = self
                    .services
                    .get(&bus_name)
                    .and_then(|service| service.items.get(&path))
                    .cloned()
                else {
                    tracing::debug!(class, instance, path, "read names no published item");
                    return Effects::default();
                };
                self.publish_item(&bus_name, &path, Some(&item))
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into()
            }
            Request::Write {
                class,
                instance,
                path,
            } => {
                if !self.mode.writes_allowed() {
                    tracing::warn!(path, "write request refused: bridge is in read-only mode");
                    return Effects::default();
                }
                let Some(bus_name) = self.unique_service(&class, instance) else {
                    tracing::warn!(class, instance, "write has no unique application target");
                    return Effects::default();
                };
                let Some(value) = payload::decode(body) else {
                    tracing::warn!(path, "write request carried no value payload");
                    return Effects::default();
                };
                let Some(application) = self
                    .services
                    .get(&bus_name)
                    .map(|service| service.application.clone())
                else {
                    return Effects::default();
                };
                Effects {
                    writes: vec![Write {
                        application,
                        path,
                        value,
                    }],
                    ..Effects::default()
                }
            }
        }
    }

    /// Time passed: emit the single device heartbeat and any deferred full
    /// republish.
    pub fn on_tick(&mut self, now: Duration) -> Effects {
        if !self.alive(now) {
            return Effects::default();
        }
        let mut publications = Vec::new();
        if self.full_pending && self.full_publish_ready(now) {
            publications.extend(self.full_publish(now));
        }
        if now >= self.next_heartbeat {
            self.next_heartbeat = now + self.timings.heartbeat;
            publications.push(self.heartbeat(now));
        }
        publications.into()
    }

    /// When [`Self::on_tick`] next has work due.
    pub fn next_wake(&self, now: Duration) -> Option<Duration> {
        if !self.alive(now) {
            return None;
        }
        let deferred = self
            .full_pending
            .then(|| self.full_publish_due())
            .flatten()
            .unwrap_or(self.next_heartbeat);
        Some(self.next_heartbeat.min(deferred))
    }

    fn unique_service(&self, class: &str, instance: u64) -> Option<String> {
        let mut matching = self.services.iter().filter(|(_, service)| {
            service.application.class() == class && topic::instance_of(&service.items) == instance
        });
        let (name, _) = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        Some(name.clone())
    }

    fn service_is_unique(&self, bus_name: &str) -> bool {
        let Some(service) = self.services.get(bus_name) else {
            return false;
        };
        self.unique_service(
            service.application.class(),
            topic::instance_of(&service.items),
        )
        .as_deref()
            == Some(bus_name)
    }

    fn clear_all_publications(&mut self) {
        for service in self.services.values_mut() {
            self.pending_clears
                .extend(std::mem::take(&mut service.published));
        }
    }

    fn full_publish_ready(&self, now: Duration) -> bool {
        self.full_publish_due().is_none_or(|due| now >= due)
    }

    fn full_publish_due(&self) -> Option<Duration> {
        self.last_full
            .map(|last| last + self.timings.full_publish_min_interval)
    }

    fn request_full(&mut self, now: Duration) -> Vec<Publication> {
        if !self.alive(now) {
            return Vec::new();
        }
        if self.full_publish_ready(now) {
            return self.full_publish(now);
        }
        self.full_pending = true;
        Vec::new()
    }

    /// Clear obsolete retained addresses immediately while alive, even when
    /// the full republish itself is rate-limited. A broker must never keep a
    /// vanished or newly-colliding application visible for the rate-limit
    /// interval.
    fn after_topology_change(&mut self, now: Duration) -> Vec<Publication> {
        let mut publications = if self.alive(now) {
            self.flush_clears()
        } else {
            Vec::new()
        };
        publications.extend(self.request_full(now));
        publications
    }

    fn full_publish(&mut self, now: Duration) -> Vec<Publication> {
        let mut publications = self.flush_clears();
        self.last_full = Some(now);
        self.full_pending = false;

        let publishable: Vec<(String, String, Item)> = self
            .services
            .iter()
            .filter(|(name, _)| self.service_is_unique(name))
            .flat_map(|(name, service)| {
                service
                    .items
                    .iter()
                    .map(|(path, item)| (name.clone(), path.clone(), item.clone()))
            })
            .collect();
        let mut item_count = 0;
        for (name, path, item) in publishable {
            if let Some(publication) = self.publish_item(&name, &path, Some(&item)) {
                publications.push(publication);
                item_count += 1;
            }
        }
        publications.push(Publication {
            topic: format!(
                "{}/{}/{}",
                topic::NOTIFY,
                self.device_id,
                topic::FULL_PUBLISH_COMPLETED
            ),
            payload: payload::value_only(Json::from(item_count)),
            retain: false,
        });
        publications
    }

    fn flush_clears(&mut self) -> Vec<Publication> {
        std::mem::take(&mut self.pending_clears)
            .into_iter()
            .map(|topic| Publication {
                topic,
                payload: payload::CLEAR.to_vec(),
                retain: true,
            })
            .collect()
    }

    fn publish_item(
        &mut self,
        bus_name: &str,
        path: &str,
        item: Option<&Item>,
    ) -> Option<Publication> {
        if !self.service_is_unique(bus_name) {
            return None;
        }
        let service = self.services.get(bus_name)?;
        let topic = service
            .address(&self.device_id)
            .item_topic(topic::NOTIFY, path);
        let body = match item {
            Some(item) => payload::encode(path, item),
            None => payload::invalid(),
        };
        self.services
            .get_mut(bus_name)?
            .published
            .insert(topic.clone());
        Some(Publication {
            topic,
            payload: body,
            retain: true,
        })
    }

    fn heartbeat(&self, now: Duration) -> Publication {
        Publication {
            topic: format!("{}/{}/{}", topic::NOTIFY, self.device_id, topic::HEARTBEAT),
            payload: payload::value_only(Json::from(now.as_secs())),
            retain: false,
        }
    }
}
