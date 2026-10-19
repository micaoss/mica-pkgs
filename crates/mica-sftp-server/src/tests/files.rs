//! Reading, writing, creating and opening files.

use super::super::Session;
use russh_sftp::protocol::{
    Close, FileAttributes, Open, OpenFlags, Packet, Read, Stat, StatusCode, Write,
};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

#[test]
pub(super) fn a_written_file_reads_back_at_its_offsets_and_then_reports_eof() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("upload.bin");
    let mut session = Session::default();

    let handle = handle_of(open(
        &mut session,
        1,
        &file,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
    ));
    for (id, offset, data) in [(2, 0, &b"hello "[..]), (3, 6, &b"world"[..])] {
        let reply = call(
            &mut session,
            Packet::Write(Write {
                id,
                handle: handle.clone(),
                offset,
                data: data.to_vec(),
            }),
        );
        assert_eq!(status_of(&reply), StatusCode::Ok);
    }
    let closed = call(&mut session, Packet::Close(Close { id: 4, handle }));
    assert_eq!(status_of(&closed), StatusCode::Ok);
    assert_eq!(std::fs::read(&file).unwrap(), b"hello world");

    let handle = handle_of(open(&mut session, 5, &file, OpenFlags::READ));
    let data = call(
        &mut session,
        Packet::Read(Read {
            id: 6,
            handle: handle.clone(),
            offset: 6,
            len: 32768,
        }),
    );
    match data {
        Packet::Data(data) => {
            assert_eq!(data.id, 6);
            assert_eq!(data.data, b"world");
        }
        other => panic!("expected data, got {other:?}"),
    }
    let eof = call(
        &mut session,
        Packet::Read(Read {
            id: 7,
            handle,
            offset: 11,
            len: 32768,
        }),
    );
    assert_eq!(status_of(&eof), StatusCode::Eof);
}

#[test]
pub(super) fn a_read_at_the_end_of_the_offset_space_is_answered_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "x").unwrap();
    let mut session = Session::default();
    let handle = handle_of(open(&mut session, 1, &file, OpenFlags::READ));

    let reply = call(
        &mut session,
        Packet::Read(Read {
            id: 2,
            handle,
            offset: u64::MAX,
            len: 32768,
        }),
    );

    assert!(matches!(
        status_of(&reply),
        StatusCode::Eof | StatusCode::Failure
    ));
}

#[test]
pub(super) fn a_closed_or_unknown_handle_is_a_failure_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "x").unwrap();
    let mut session = Session::default();
    let handle = handle_of(open(&mut session, 1, &file, OpenFlags::READ));
    call(
        &mut session,
        Packet::Close(Close {
            id: 2,
            handle: handle.clone(),
        }),
    );

    let reply = call(
        &mut session,
        Packet::Read(Read {
            id: 3,
            handle,
            offset: 0,
            len: 10,
        }),
    );

    assert_eq!(status_of(&reply), StatusCode::Failure);
}

#[test]
pub(super) fn an_exclusive_create_of_an_existing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("exists");
    std::fs::write(&file, "keep").unwrap();
    let mut session = Session::default();

    let reply = open(
        &mut session,
        1,
        &file,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
    );

    assert_eq!(status_of(&reply), StatusCode::Failure);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
}

#[test]
pub(super) fn a_create_takes_the_requested_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("private");
    let mut session = Session::default();

    let reply = call(
        &mut session,
        Packet::Open(Open {
            id: 1,
            filename: path(&file),
            pflags: OpenFlags::WRITE | OpenFlags::CREATE,
            attrs: FileAttributes {
                permissions: Some(0o600),
                ..FileAttributes::default()
            },
        }),
    );

    handle_of(reply);
    assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
}

#[test]
pub(super) fn a_missing_file_is_no_such_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::default();

    let reply = open(&mut session, 1, &dir.path().join("absent"), OpenFlags::READ);
    let stat = call(
        &mut session,
        Packet::Stat(Stat {
            id: 2,
            path: path(&dir.path().join("absent")),
        }),
    );

    assert_eq!(status_of(&reply), StatusCode::NoSuchFile);
    assert_eq!(status_of(&stat), StatusCode::NoSuchFile);
}

#[test]
pub(super) fn a_mode_the_user_lacks_is_permission_denied() {
    let dir = tempfile::tempdir().unwrap();
    if modes_are_bypassed(dir.path()) {
        eprintln!("skipped: this process is not held to file modes (root or CAP_DAC_OVERRIDE)");
        return;
    }
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut session = Session::default();

    let reply = open(
        &mut session,
        1,
        &locked.join("new"),
        OpenFlags::WRITE | OpenFlags::CREATE,
    );

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(status_of(&reply), StatusCode::PermissionDenied);
}
