//! The topic grammar: `N|R|W/<deviceId>/<class>/<instance>/<path>`.
//!
//! Three verbs, and the direction is part of the verb: `N` is what the bridge
//! publishes, `R` and `W` are what it subscribes to. Building and parsing both
//! live here so the two can never drift apart.

use crate::item::Item;

/// Notification: device -> broker. The only verb this bridge publishes under.
pub const NOTIFY: &str = "N";
/// Read request: broker -> device. The bridge republishes the addressed item.
pub const READ: &str = "R";
/// Write request: broker -> device. Carried through to `SetValue` in full
/// mode, refused in read-only mode.
pub const WRITE: &str = "W";

/// `R/<deviceId>/keepalive` — arms the alive window and asks for a full
/// republish (the dbus-flashmq shape). Any payload it carries is ignored:
/// this bridge answers a keepalive with a full republish, never a partial
/// one.
pub const KEEPALIVE: &str = "keepalive";
/// `N/<deviceId>/full_publish_completed` — the marker that terminates a full
/// republish.
pub const FULL_PUBLISH_COMPLETED: &str = "full_publish_completed";
/// `N/<deviceId>/heartbeat` — the 3 s liveness beat, published only while the
/// alive window holds.
pub const HEARTBEAT: &str = "heartbeat";

/// A bus service admitted to the MQTT application data plane.
///
/// Construction is deliberately private to package enrollment. A raw D-Bus
/// name observed at runtime cannot be smuggled into bridge state by pairing it
/// with an application-looking class string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application {
    bus_name: String,
    class: String,
}

impl Application {
    /// Construct one application from an exact package enrollment.
    pub(crate) fn from_enrollment(bus_name: &str) -> anyhow::Result<Self> {
        zbus::names::WellKnownName::try_from(bus_name)
            .map_err(|error| anyhow::anyhow!("not a valid D-Bus service name: {error}"))?;
        let parsed = mica_busname::parse(bus_name)
            .ok_or_else(|| anyhow::anyhow!("not a com.mica.<class>[.<suffix>] service name"))?;
        if !valid_topic_segment(parsed.class) {
            anyhow::bail!("class is not a safe MQTT topic segment");
        }
        Ok(Self {
            bus_name: bus_name.to_string(),
            class: parsed.class.to_string(),
        })
    }

    /// The exact well-known D-Bus name to read and write.
    pub fn bus_name(&self) -> &str {
        &self.bus_name
    }

    /// The MQTT class derived by the shared bus-name parser.
    pub fn class(&self) -> &str {
        &self.class
    }
}

/// The three topic segments between the verb and the item path.
///
/// `class` is the admitted application's class: the third component of
/// `com.mica.<class>[.<suffix>]`. `instance` is the service's
/// `/DeviceInstance`, or `0` when a non-conforming application omits one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub device_id: String,
    pub class: String,
    pub instance: u64,
}

impl Address {
    /// The topic one item is addressed by under `verb`. `path` is the item's
    /// absolute slash path, whose leading slash is also the separator the
    /// topic needs.
    pub fn item_topic(&self, verb: &str, path: &str) -> String {
        let Self {
            device_id,
            class,
            instance,
            ..
        } = self;
        format!("{verb}/{device_id}/{class}/{instance}{path}")
    }
}

/// A request the broker addressed to this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `R/<deviceId>/keepalive`.
    Keepalive,
    /// `R/<deviceId>/<class>/<instance>/<path>`.
    Read {
        class: String,
        instance: u64,
        path: String,
    },
    /// `W/<deviceId>/<class>/<instance>/<path>`.
    Write {
        class: String,
        instance: u64,
        path: String,
    },
}

/// The request `topic` carries, or `None` when it is not one of ours.
///
/// This function validates the grammar and device identity. The bridge then
/// resolves class and instance against its admitted application set; a valid
/// topic with no unique application target is ignored there.
pub fn parse(topic: &str, device_id: &str) -> Option<Request> {
    let mut segments = topic.split('/');
    let verb = segments.next()?;
    if segments.next()? != device_id {
        return None;
    }
    let rest: Vec<&str> = segments.collect();
    if verb == READ && rest == [KEEPALIVE] {
        return Some(Request::Keepalive);
    }
    let [class, instance, path @ ..] = rest.as_slice() else {
        return None;
    };
    let instance = instance.parse::<u64>().ok()?;
    if path.is_empty() {
        return None;
    }
    let path = format!("/{}", path.join("/"));
    if !valid_item_path(&path) {
        return None;
    }
    match verb {
        READ => Some(Request::Read {
            class: (*class).to_string(),
            instance,
            path,
        }),
        WRITE => Some(Request::Write {
            class: (*class).to_string(),
            instance,
            path,
        }),
        _ => None,
    }
}

/// Whether `segment` can safely occupy one MQTT topic level.
pub fn valid_topic_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

/// Whether an application item key is a real D-Bus object path.
pub fn valid_item_path(path: &str) -> bool {
    zbus::ObjectPath::try_from(path).is_ok()
}

/// The `class` a `com.mica.*` bus name declares, or `None` when the name is not
/// one: the third component of `com.mica.<class>[.<suffix>]`.
///
/// The rule itself lives in [`mica_busname`] and only there. micad's service
/// registry derives the same class from the same names, and a second copy
/// here would be a second rule that could classify a service differently.
pub fn class_of(bus_name: &str) -> Option<&str> {
    mica_busname::parse(bus_name).map(|name| name.class)
}

/// The `/DeviceInstance` an item map declares, or `0` when it declares none.
///
/// `/DeviceInstance` is mandatory for applications. The `0` fallback keeps a
/// non-conforming application observable, while collision handling prevents
/// ambiguous reads or writes.
pub fn instance_of(items: &std::collections::BTreeMap<String, Item>) -> u64 {
    items
        .get("/DeviceInstance")
        .and_then(|item| item.value.as_u64())
        .unwrap_or(0)
}
