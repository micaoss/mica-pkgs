//! `mica-mqttd` — the MQTT application-data bridge.
//!
//! The bridge discovers only exact `com.mica.*` names installed in its
//! package-owned enrollment directory and knows their `GetItems`,
//! `ItemsChanged` and `SetValue` application surface. It never calls or
//! subscribes to micad. Device identity arrives as runtime configuration, so no
//! system setting, state, signal, method or action becomes an MQTT item.
//!
//! # The protocol is the mica-native grammar, and only that
//!
//! # Shape: a state machine, a transport, and a source

#![forbid(unsafe_code)]

pub mod bridge;
pub mod config;
pub mod enrollment;
pub mod item;
pub mod payload;
pub mod runtime;
pub mod source;
pub mod topic;
pub mod transport;
