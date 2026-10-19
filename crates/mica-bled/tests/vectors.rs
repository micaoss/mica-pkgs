//! `vectors.json` is mica-app's `test/protocol/vectors.json`, byte for byte:
//! both ends check the same derivations.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mica_bled::keys::{AAD, Sealer, password_scalar, session_keys};
use mica_bled::spake2::{Role, Spake2};
use p256::FieldBytes;
use p256::elliptic_curve::ff::PrimeField;
use serde_json::Value;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors.json")).unwrap()
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap()
}

fn scalar(h: &str) -> p256::Scalar {
    let bytes: [u8; 32] = hex(h).try_into().unwrap();
    Option::from(p256::Scalar::from_repr(FieldBytes::from(bytes))).unwrap()
}

#[test]
fn password_scalars() {
    let v = vectors();
    let device = s(&v, "deviceId");
    let w_pin = password_scalar(device, s(&v, "pin").as_bytes());
    assert_eq!(w_pin.to_repr().to_vec(), hex(s(&v, "wPin")));
    let w_phone = password_scalar(device, &hex(s(&v, "phoneSecret")));
    assert_eq!(w_phone.to_repr().to_vec(), hex(s(&v, "wPhone")));
}

#[test]
fn spake2_exchange() {
    let v = vectors();
    let sp = &v["spake2"];
    assert_eq!(s(sp, "aad").as_bytes(), AAD);
    let w = password_scalar(s(&v, "deviceId"), s(&v, "pin").as_bytes());
    let (id_a, id_b) = (s(sp, "idA").as_bytes(), s(sp, "idB").as_bytes());
    let a = Spake2::new(Role::A, id_a, id_b, w, scalar(s(sp, "x")));
    let b = Spake2::new(Role::B, id_a, id_b, w, scalar(s(sp, "y")));
    assert_eq!(a.message(), hex(s(sp, "pA")));
    assert_eq!(b.message(), hex(s(sp, "pB")));
    let (pa, pb) = (a.message().to_vec(), b.message().to_vec());
    let ka = a.finish(&pb, AAD).unwrap();
    let kb = b.finish(&pa, AAD).unwrap();
    assert_eq!(kb.ke.to_vec(), hex(s(sp, "ke")));
    assert_eq!(ka.confirmation(), hex(s(sp, "cA")));
    assert_eq!(kb.confirmation(), hex(s(sp, "cB")));
    assert!(kb.verify(&hex(s(sp, "cA"))));
}

#[test]
fn session_keys_and_sealing() {
    let v = vectors();
    let session = &v["session"];
    let keys = session_keys(&hex(s(&v["spake2"], "ke")));
    assert_eq!(
        keys.phone_to_device.to_vec(),
        hex(s(session, "phoneToDevice"))
    );
    assert_eq!(
        keys.device_to_phone.to_vec(),
        hex(s(session, "deviceToPhone"))
    );
    let plaintext = s(session, "plaintext").as_bytes();
    let mut rx = Sealer::new(&keys.phone_to_device);
    assert_eq!(
        rx.open(&hex(s(session, "sealedCounter0"))).unwrap(),
        plaintext
    );
    assert_eq!(
        rx.open(&hex(s(session, "sealedCounter1"))).unwrap(),
        plaintext
    );
    let mut tx = Sealer::new(&keys.phone_to_device);
    assert_eq!(
        tx.seal(plaintext).unwrap(),
        hex(s(session, "sealedCounter0"))
    );
}
