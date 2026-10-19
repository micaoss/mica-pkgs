//! Protocol unit tests: requests built and replies decoded with russh-sftp's
//! own packet types, so the bytes on the wire are the ones a client sends.

use super::Session;
use bytes::{Buf, Bytes};
use russh_sftp::protocol::{FileAttributes, Init, Open, OpenFlags, Packet, StatusCode};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

mod files;
mod metadata;
mod protocol;

/// Encode `packet` the way a client puts it on the wire: length, type, body.
fn wire(packet: Packet) -> Vec<u8> {
    Bytes::try_from(packet).expect("request encodes").to_vec()
}

/// One request through a session, one decoded reply back.
fn call(session: &mut Session, packet: Packet) -> Packet {
    let mut bytes = Bytes::from(wire(packet));
    bytes.advance(4);
    let reply = session.dispatch(bytes);
    let mut encoded = Bytes::try_from(reply).expect("reply encodes");
    encoded.advance(4);
    Packet::try_from(&mut encoded).expect("reply decodes")
}

fn status_of(packet: &Packet) -> StatusCode {
    match packet {
        Packet::Status(status) => status.status_code,
        other => panic!("expected a status reply, got {other:?}"),
    }
}

fn path(p: &Path) -> String {
    p.to_str().expect("temp paths are UTF-8").to_string()
}

fn open(session: &mut Session, id: u32, file: &Path, pflags: OpenFlags) -> Packet {
    call(
        session,
        Packet::Open(Open {
            id,
            filename: path(file),
            pflags,
            attrs: FileAttributes::default(),
        }),
    )
}

fn handle_of(packet: Packet) -> String {
    match packet {
        Packet::Handle(handle) => handle.handle,
        other => panic!("expected a handle, got {other:?}"),
    }
}

fn attrs_of(packet: Packet) -> FileAttributes {
    match packet {
        Packet::Attrs(attrs) => attrs.attrs,
        other => panic!("expected attributes, got {other:?}"),
    }
}

/// `S_IFMT` of the attributes' mode.
fn type_bits(attrs: &FileAttributes) -> u32 {
    attrs.permissions.expect("mode is always sent") & 0o170000
}

/// The request id a reply answers. `Packet::get_request_id` only knows
/// request types and reports 0 for every reply.
fn reply_id(packet: &Packet) -> u32 {
    match packet {
        Packet::Status(p) => p.id,
        Packet::Attrs(p) => p.id,
        Packet::Handle(p) => p.id,
        Packet::Data(p) => p.id,
        Packet::Name(p) => p.id,
        other => panic!("not a reply: {other:?}"),
    }
}

fn names_of(packet: Packet) -> Vec<russh_sftp::protocol::File> {
    match packet {
        Packet::Name(name) => name.files,
        other => panic!("expected names, got {other:?}"),
    }
}

/// True when this process is not held to file modes (root, or
/// CAP_DAC_OVERRIDE): probed rather than read from the uid, because a
/// capability grants the same thing.
fn modes_are_bypassed(dir: &Path) -> bool {
    let probe = dir.join("probe");
    std::fs::write(&probe, "x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let bypassed = std::fs::File::open(&probe).is_ok();
    std::fs::remove_file(&probe).unwrap();
    bypassed
}

#[test]
fn init_answers_version_3() {
    let mut session = Session::default();

    let reply = call(
        &mut session,
        Packet::Init(Init {
            version: 3,
            extensions: Default::default(),
        }),
    );

    match reply {
        Packet::Version(version) => assert_eq!(version.version, 3),
        other => panic!("expected a version reply, got {other:?}"),
    }
}
