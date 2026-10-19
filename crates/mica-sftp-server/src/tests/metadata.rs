//! Listing, stat, setstat and changing the tree.

use super::super::Session;
use russh_sftp::protocol::{
    FSetStat, FileAttributes, Fstat, Lstat, MkDir, OpenDir, OpenFlags, Packet, ReadDir, ReadLink,
    Remove, RmDir, SetStat, Stat, StatusCode, Symlink,
};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::*;

#[test]
pub(super) fn a_directory_lists_every_entry_with_attributes_then_reports_eof() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "12345").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let mut session = Session::default();

    let handle = handle_of(call(
        &mut session,
        Packet::OpenDir(OpenDir {
            id: 1,
            path: path(dir.path()),
        }),
    ));
    let mut names = Vec::new();
    let mut id = 2;
    loop {
        let reply = call(
            &mut session,
            Packet::ReadDir(ReadDir {
                id,
                handle: handle.clone(),
            }),
        );
        id += 1;
        match reply {
            Packet::Name(name) => names.extend(name.files),
            Packet::Status(status) => {
                assert_eq!(status.status_code, StatusCode::Eof);
                break;
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }
    names.sort_by(|a, b| a.filename.cmp(&b.filename));

    let listed: Vec<&str> = names.iter().map(|f| f.filename.as_str()).collect();
    assert_eq!(listed, ["file.txt", "sub"]);
    assert_eq!(names[0].attrs.size, Some(5));
    assert!(
        names[0].longname.starts_with("-rw"),
        "{}",
        names[0].longname
    );
    assert!(
        names[0].longname.ends_with(" file.txt"),
        "{}",
        names[0].longname
    );
    assert!(names[1].longname.starts_with('d'), "{}", names[1].longname);
    assert_eq!(type_bits(&names[1].attrs), 0o040000);
}

#[test]
pub(super) fn stat_follows_a_symlink_and_lstat_does_not() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target"), "abc").unwrap();
    let mut session = Session::default();

    // OpenSSH's client sends the TARGET first and the link path second, and
    // OpenSSH's server honours that order; russh-sftp names the two fields the
    // other way round.
    let made = call(
        &mut session,
        Packet::Symlink(Symlink {
            id: 1,
            linkpath: "target".to_string(),
            targetpath: path(&dir.path().join("link")),
        }),
    );
    assert_eq!(status_of(&made), StatusCode::Ok);
    assert_eq!(
        std::fs::read_link(dir.path().join("link")).unwrap(),
        Path::new("target")
    );

    let link = path(&dir.path().join("link"));
    let followed = attrs_of(call(
        &mut session,
        Packet::Stat(Stat {
            id: 2,
            path: link.clone(),
        }),
    ));
    let not_followed = attrs_of(call(
        &mut session,
        Packet::Lstat(Lstat {
            id: 3,
            path: link.clone(),
        }),
    ));
    let target = names_of(call(
        &mut session,
        Packet::ReadLink(ReadLink { id: 4, path: link }),
    ));

    // By the file-type bits, not russh-sftp's `is_regular`/`is_symlink`,
    // which test bit containment and so call a symlink (0o120000) regular
    // (0o100000) too.
    assert_eq!(type_bits(&followed), 0o100000);
    assert_eq!(followed.size, Some(3));
    assert_eq!(type_bits(&not_followed), 0o120000);
    assert_eq!(target.len(), 1);
    assert_eq!(target[0].filename, "target");
}

#[test]
pub(super) fn fstat_reports_the_open_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "four").unwrap();
    let mut session = Session::default();
    let handle = handle_of(open(&mut session, 1, &file, OpenFlags::READ));

    let attrs = attrs_of(call(&mut session, Packet::Fstat(Fstat { id: 2, handle })));

    assert_eq!(attrs.size, Some(4));
    assert_eq!(type_bits(&attrs), 0o100000);
}

#[test]
pub(super) fn setstat_applies_permissions_and_times_and_fsetstat_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f");
    std::fs::write(&file, "0123456789").unwrap();
    let mut session = Session::default();

    let set = call(
        &mut session,
        Packet::SetStat(SetStat {
            id: 1,
            path: path(&file),
            attrs: FileAttributes {
                permissions: Some(0o640),
                atime: Some(1_000_000),
                mtime: Some(2_000_000),
                ..FileAttributes::default()
            },
        }),
    );
    assert_eq!(status_of(&set), StatusCode::Ok);
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o640);
    assert_eq!(meta.atime(), 1_000_000);
    assert_eq!(meta.mtime(), 2_000_000);

    let handle = handle_of(open(&mut session, 2, &file, OpenFlags::WRITE));
    let truncated = call(
        &mut session,
        Packet::FSetStat(FSetStat {
            id: 3,
            handle,
            attrs: FileAttributes {
                size: Some(4),
                ..FileAttributes::default()
            },
        }),
    );
    assert_eq!(status_of(&truncated), StatusCode::Ok);
    assert_eq!(std::fs::read(&file).unwrap(), b"0123");
}

#[test]
pub(super) fn mkdir_rmdir_and_remove_change_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    let file = dir.path().join("gone");
    std::fs::write(&file, "x").unwrap();
    let mut session = Session::default();

    let made = call(
        &mut session,
        Packet::MkDir(MkDir {
            id: 1,
            path: path(&sub),
            attrs: FileAttributes {
                permissions: Some(0o750),
                ..FileAttributes::default()
            },
        }),
    );
    assert_eq!(status_of(&made), StatusCode::Ok);
    assert!(sub.is_dir());
    assert_eq!(std::fs::metadata(&sub).unwrap().mode() & 0o777, 0o750);

    let again = call(
        &mut session,
        Packet::MkDir(MkDir {
            id: 2,
            path: path(&sub),
            attrs: FileAttributes::default(),
        }),
    );
    assert_eq!(status_of(&again), StatusCode::Failure);

    let removed_dir = call(
        &mut session,
        Packet::RmDir(RmDir {
            id: 3,
            path: path(&sub),
        }),
    );
    let removed_file = call(
        &mut session,
        Packet::Remove(Remove {
            id: 4,
            filename: path(&file),
        }),
    );
    assert_eq!(status_of(&removed_dir), StatusCode::Ok);
    assert_eq!(status_of(&removed_file), StatusCode::Ok);
    assert!(!sub.exists());
    assert!(!file.exists());
}
