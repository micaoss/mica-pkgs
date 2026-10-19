//! `/usr/lib/sftp-server`: one SFTP session on stdin/stdout.
//!
//! dropbear execs this for the `sftp` subsystem as the logged-in user, with no
//! arguments; any given are ignored. Errors go to stderr, which the SSH server
//! forwards to the client.

#![forbid(unsafe_code)]

use std::io::{BufWriter, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let stdin = std::io::stdin().lock();
    let mut stdout = BufWriter::new(std::io::stdout().lock());
    let served = mica_sftp_server::serve(stdin, &mut stdout).and_then(|()| stdout.flush());
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sftp-server: {err}");
            ExitCode::FAILURE
        }
    }
}
