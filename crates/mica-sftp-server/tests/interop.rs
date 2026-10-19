//! The binary against OpenSSH's own client: `sftp -D <binary>` speaks SFTP to
//! the server over a pipe, with no SSH connection in between, so these runs are
//! the client's real request sequences (pipelined requests, chunked transfers,
//! `ls -l` long names) rather than packets this crate built itself.
//!
//! Skipped with a message when `sftp` is not on PATH.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SERVER: &str = env!("CARGO_BIN_EXE_mica-sftp-server");

fn sftp() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("sftp"))
        .find(|candidate| candidate.is_file())
}

/// Run `batch` through `sftp -b` against the server, from `dir`.
fn run_batch(sftp: &Path, dir: &Path, batch: &str) -> Output {
    let batch_file = dir.join("batch");
    std::fs::write(&batch_file, batch).unwrap();
    Command::new(sftp)
        .arg("-b")
        .arg(&batch_file)
        .arg("-D")
        .arg(SERVER)
        .current_dir(dir)
        .output()
        .expect("sftp runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Content spanning several 32 KiB transfer chunks, and not a repeating byte,
/// so a chunk written at the wrong offset shows.
fn payload() -> Vec<u8> {
    (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn upload_download_listing_rename_mkdir_rmdir_and_symlink() {
    let Some(sftp) = sftp() else {
        eprintln!("skipped: no sftp client on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("local");
    let remote = dir.path().join("remote");
    std::fs::create_dir(&local).unwrap();
    std::fs::create_dir(&remote).unwrap();
    std::fs::write(local.join("upload.bin"), payload()).unwrap();
    std::fs::set_permissions(
        local.join("upload.bin"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();

    let batch = format!(
        "lcd {local}\n\
         cd {remote}\n\
         put -p upload.bin\n\
         get upload.bin download.bin\n\
         mkdir made\n\
         ls -l\n\
         rename upload.bin renamed.bin\n\
         ln -s renamed.bin link.bin\n\
         rmdir made\n\
         ls\n",
        local = local.display(),
        remote = remote.display(),
    );
    let output = run_batch(&sftp, dir.path(), &batch);
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert!(
        output.status.success(),
        "sftp failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        std::fs::read(local.join("download.bin")).unwrap(),
        payload()
    );
    assert_eq!(
        std::fs::read(remote.join("renamed.bin")).unwrap(),
        payload()
    );
    assert!(!remote.join("upload.bin").exists());
    assert!(!remote.join("made").exists());
    assert_eq!(
        std::fs::metadata(remote.join("renamed.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640,
        "put -p carries the mode through setstat"
    );
    assert_eq!(
        std::fs::read_link(remote.join("link.bin")).unwrap(),
        Path::new("renamed.bin"),
        "ln -s names the target first, the way OpenSSH's server reads it"
    );
    let long_listing = stdout
        .lines()
        .find(|line| line.ends_with(" upload.bin") && line.starts_with("-rw-r-----"));
    assert!(
        long_listing.is_some(),
        "ls -l shows the uploaded file's long name:\n{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with('d') && line.ends_with(" made")),
        "ls -l shows the made directory:\n{stdout}"
    );
    assert!(stdout.contains("renamed.bin"), "{stdout}");
}

#[test]
fn a_write_the_account_may_not_make_is_permission_denied() {
    let Some(sftp) = sftp() else {
        eprintln!("skipped: no sftp client on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(dir.path().join("upload.txt"), "x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(locked.join("probe"), "x").is_ok() {
        eprintln!("skipped: this process is not held to file modes (root or CAP_DAC_OVERRIDE)");
        return;
    }

    let batch = format!(
        "put {upload} {target}\n",
        upload = dir.path().join("upload.txt").display(),
        target = locked.join("upload.txt").display(),
    );
    let output = run_batch(&sftp, dir.path(), &batch);

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "the batch must fail");
    assert!(
        stderr.contains("Permission denied"),
        "the client reports the status it was sent:\n{stderr}"
    );
    assert!(!locked.join("upload.txt").exists());
}
