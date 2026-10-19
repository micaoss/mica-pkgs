//! `mica-busname` — the one rule that turns a `com.mica.*` service name into
//! the `<class>` shared by the service registry and MQTT addressing.
//!
//! Every service uses one grammar:
//!
//! ```text
//! com.mica.<class>[.<suffix>]
//! ```
//!
//! A service name identifies a process endpoint. It does not say whether the
//! service is system management or whether its data may be bridged to MQTT;
//! those are interface, enrollment, and authorization decisions made by the
//! consumers. This crate deliberately has no dependencies.

#![forbid(unsafe_code)]

/// The prefix every mica service name carries.
pub const PREFIX: &str = "com.mica.";

/// A parsed `com.mica.*` service name, borrowing from the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusName<'a> {
    /// The third dotted component and the MQTT class when the service is
    /// explicitly enrolled in the application-data plane.
    pub class: &'a str,
    /// Everything after the class, with the separating dot dropped.
    pub suffix: Option<&'a str>,
}

/// Parse `bus_name` under `com.mica.<class>[.<suffix>]`.
///
/// Returns `None` for names outside `com.mica.`, the bare namespace, or a name
/// containing an empty component. Whenever this returns `Some`, both the
/// class and every suffix component are non-empty.
pub fn parse(bus_name: &str) -> Option<BusName<'_>> {
    let rest = bus_name.strip_prefix(PREFIX)?;
    if rest.is_empty() || rest.split('.').any(str::is_empty) {
        return None;
    }
    let (class, suffix) = match rest.split_once('.') {
        Some((class, suffix)) => (class, Some(suffix)),
        None => (rest, None),
    };
    Some(BusName { class, suffix })
}

#[cfg(test)]
mod tests {
    use super::{BusName, parse};

    fn service<'a>(class: &'a str, suffix: Option<&'a str>) -> Option<BusName<'a>> {
        Some(BusName { class, suffix })
    }

    #[test]
    fn every_service_carries_its_class_in_the_third_component() {
        assert_eq!(parse("com.mica.micad"), service("micad", None));
        assert_eq!(
            parse("com.mica.sensor.abc123"),
            service("sensor", Some("abc123"))
        );
        assert_eq!(parse("com.mica.sensor.a.b"), service("sensor", Some("a.b")));
    }

    #[test]
    fn ext_has_no_namespace_semantics() {
        assert_eq!(parse("com.mica.ext"), service("ext", None));
        assert_eq!(parse("com.mica.ext.sensor"), service("ext", Some("sensor")));
        assert_eq!(parse("com.mica.extra"), service("extra", None));
    }

    #[test]
    fn names_outside_the_grammar_are_refused() {
        for name in [
            "com.mica.",
            "com.mica",
            "com.example.foo",
            "",
            "com.mica.sensor.",
            "com.mica..sensor",
            "com.mica.ext.",
        ] {
            assert_eq!(parse(name), None, "{name}");
        }
    }
}
