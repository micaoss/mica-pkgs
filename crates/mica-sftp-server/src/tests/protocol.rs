//! Rename, realpath and the packet stream.

use super::super::{MAX_PACKET_LENGTH, Session, serve};
use bytes::{Buf, Bytes};
use russh_sftp::protocol::{Init, Packet, RealPath, Rename, Stat, StatusCode};

use super::*;

#[test]
pub(super) fn rename_moves_a_file_and_refuses_to_replace_an_existing_one() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    let new = dir.path().join("new");
    let taken = dir.path().join("taken");
    std::fs::write(&old, "moved").unwrap();
    std::fs::write(&taken, "keep").unwrap();
    let mut session = Session::default();

    let moved = call(
        &mut session,
        Packet::Rename(Rename {
            id: 1,
            oldpath: path(&old),
            newpath: path(&new),
        }),
    );
    let refused = call(
        &mut session,
        Packet::Rename(Rename {
            id: 2,
            oldpath: path(&new),
            newpath: path(&taken),
        }),
    );

    assert_eq!(status_of(&moved), StatusCode::Ok);
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "moved");
    assert!(!old.exists());
    assert_eq!(status_of(&refused), StatusCode::Failure);
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "keep");
    assert!(new.exists());
}

#[test]
pub(super) fn realpath_resolves_dot_symlinks_and_a_missing_last_component() {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(real.join("sub")).unwrap();
    std::os::unix::fs::symlink(real.join("sub"), real.join("via")).unwrap();
    let mut session = Session::default();

    let resolve = |session: &mut Session, id: u32, p: String| {
        let files = names_of(call(session, Packet::RealPath(RealPath { id, path: p })));
        assert_eq!(files.len(), 1);
        files[0].filename.clone()
    };

    assert_eq!(
        resolve(&mut session, 1, ".".to_string()),
        path(&std::env::current_dir().unwrap())
    );
    assert_eq!(
        resolve(&mut session, 2, String::new()),
        path(&std::env::current_dir().unwrap())
    );
    assert_eq!(
        resolve(&mut session, 3, format!("{}/via/../sub/.", path(&real))),
        path(&real.join("sub"))
    );
    assert_eq!(
        resolve(&mut session, 4, format!("{}/via/new-file", path(&real))),
        path(&real.join("sub").join("new-file"))
    );
}

#[test]
pub(super) fn an_unknown_packet_type_is_unsupported_under_its_own_request_id() {
    let mut session = Session::default();
    // Type 99, request id 42, no body.
    let body = Bytes::from_static(&[99, 0, 0, 0, 42]);

    let mut encoded = Bytes::try_from(session.dispatch(body)).unwrap();
    encoded.advance(4);
    let reply = Packet::try_from(&mut encoded).unwrap();

    assert_eq!(status_of(&reply), StatusCode::OpUnsupported);
    assert_eq!(reply_id(&reply), 42);
}

#[test]
pub(super) fn serve_answers_each_request_in_order_and_ends_cleanly_at_eof() {
    let dir = tempfile::tempdir().unwrap();
    let mut input = wire(Packet::Init(Init {
        version: 3,
        extensions: Default::default(),
    }));
    input.extend(wire(Packet::Stat(Stat {
        id: 7,
        path: path(dir.path()),
    })));
    let mut output = Vec::new();

    serve(input.as_slice(), &mut output).expect("EOF after whole packets is a clean end");

    let mut replies = Bytes::from(output);
    let mut decoded = Vec::new();
    while replies.has_remaining() {
        let length = replies.get_u32() as usize;
        let mut one = replies.split_to(length);
        decoded.push(Packet::try_from(&mut one).unwrap());
    }
    assert_eq!(decoded.len(), 2);
    assert!(matches!(decoded[0], Packet::Version(_)));
    assert_eq!(reply_id(&decoded[1]), 7);
    assert!(matches!(decoded[1], Packet::Attrs(_)));
}

#[test]
pub(super) fn serve_refuses_a_packet_longer_than_the_limit() {
    let input = (MAX_PACKET_LENGTH + 1).to_be_bytes();
    let mut output = Vec::new();

    let err = serve(&input[..], &mut output).expect_err("an oversized packet ends the session");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(output.is_empty());
}
