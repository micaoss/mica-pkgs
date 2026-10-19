//! `mica-bled`: the BLE management service (mica-app's `docs/protocol.md`).

#![forbid(unsafe_code)]

pub mod allowlist;
pub mod apid;
pub mod backoff;
pub mod framing;
pub mod gatt;
pub mod keys;
pub mod phones;
pub mod pin;
pub mod session;
pub mod spake2;
