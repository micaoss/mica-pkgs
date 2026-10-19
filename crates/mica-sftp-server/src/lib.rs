//! An SFTP version 3 server on stdin/stdout: the `/usr/lib/sftp-server`
//! dropbear execs for the `sftp` subsystem, as the logged-in user.
//!
//! It does no privilege handling of its own. Every request is a plain system
//! call made with the credentials the SSH server started it with, so what a
//! session may touch is exactly what that account may touch in a shell.
//!
//! The wire format is russh-sftp's `protocol` module. The loop is this
//! crate's own rather than `russh_sftp::server::run`, which logs a framing
//! error and keeps reading from the middle of a packet; here an oversized
//! request or a failed read ends the session. Paths travel as UTF-8, a
//! limitation of that module: a file name that is not UTF-8 is listed with
//! replacement characters and cannot be addressed.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use russh_sftp::protocol::{
    Attrs, Data, File as NameEntry, FileAttributes, Handle as HandleReply, Name, OpenFlags, Packet,
    Status, StatusCode, Version,
};

/// Largest request accepted, in bytes: OpenSSH's `SFTP_MAX_MSG_LENGTH`.
pub const MAX_PACKET_LENGTH: u32 = 256 * 1024;
/// Largest read answered, leaving room for the reply's own header inside
/// [`MAX_PACKET_LENGTH`]. A client asking for more gets a short read, which
/// SFTP allows.
const MAX_READ_LENGTH: u32 = MAX_PACKET_LENGTH - 1024;
/// Directory entries per `SSH_FXP_NAME` reply, as OpenSSH's sftp-server sends.
const READDIR_BATCH: usize = 100;
/// `SSH_FXP_INIT`, the one request without a request id.
const SSH_FXP_INIT: u8 = 1;
/// `SSH_FXP_EXTENDED`.
const SSH_FXP_EXTENDED: u8 = 200;

/// Serve one SFTP session until `input` ends.
///
/// A clean end of input between packets is the client closing the session and
/// returns `Ok`.
///
/// # Errors
pub fn serve(mut input: impl Read, mut output: impl Write) -> io::Result<()> {
    let mut session = Session::default();
    loop {
        let mut length = [0u8; 4];
        match input.read_exact(&mut length) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        }
        let length = u32::from_be_bytes(length);
        if length > MAX_PACKET_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request of {length} bytes exceeds the {MAX_PACKET_LENGTH}-byte limit"),
            ));
        }
        let mut body = vec![0u8; length as usize];
        input.read_exact(&mut body)?;
        let reply = session.dispatch(Bytes::from(body));
        let encoded = Bytes::try_from(reply).map_err(|err| io::Error::other(err.to_string()))?;
        output.write_all(&encoded)?;
        output.flush()?;
    }
}

/// What a handle string refers to.
enum Handle {
    File(File),
    /// The path is kept for `fstat`/`fsetstat` on a directory handle.
    Dir {
        path: PathBuf,
        entries: fs::ReadDir,
    },
}

/// A request that did not succeed, as the status it is answered with.
struct Failure {
    code: StatusCode,
    message: String,
}

impl Failure {
    fn new(code: StatusCode) -> Self {
        Self {
            code,
            message: code.to_string(),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(err: io::Error) -> Self {
        let code = match err.kind() {
            io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        };
        Self {
            code,
            message: err.to_string(),
        }
    }
}

type Reply = Result<Packet, Failure>;

/// Per-session state: the open handles.
#[derive(Default)]
pub struct Session {
    handles: HashMap<String, Handle>,
    next_handle: u64,
}

impl Session {
    /// Answer one request body, from the type byte onwards.
    pub fn dispatch(&mut self, mut body: Bytes) -> Packet {
        let raw_id = raw_request_id(&body);
        if !body.first().is_some_and(|kind| is_request_type(*kind)) {
            return Packet::error(raw_id, StatusCode::OpUnsupported);
        }
        let Ok(request) = Packet::try_from(&mut body) else {
            return Packet::error(raw_id, StatusCode::BadMessage);
        };
        let id = request.get_request_id();
        let reply = match request {
            Packet::Init(_) => return Packet::Version(Version::new()),
            Packet::Open(r) => self.open(id, &r.filename, r.pflags, &r.attrs),
            Packet::Close(r) => self.close(id, &r.handle),
            Packet::Read(r) => self.read(id, &r.handle, r.offset, r.len),
            Packet::Write(r) => self.write(id, &r.handle, r.offset, &r.data),
            Packet::Lstat(r) => fs::symlink_metadata(&r.path)
                .map(|m| attrs(id, &m))
                .map_err(Into::into),
            Packet::Stat(r) => fs::metadata(&r.path)
                .map(|m| attrs(id, &m))
                .map_err(Into::into),
            Packet::Fstat(r) => self.fstat(id, &r.handle),
            Packet::SetStat(r) => {
                set_path_attributes(Path::new(&r.path), &r.attrs).map(|()| ok(id))
            }
            Packet::FSetStat(r) => self.fsetstat(id, &r.handle, &r.attrs),
            Packet::OpenDir(r) => self.opendir(id, &r.path),
            Packet::ReadDir(r) => self.readdir(id, &r.handle),
            Packet::Remove(r) => fs::remove_file(&r.filename)
                .map(|()| ok(id))
                .map_err(Into::into),
            Packet::MkDir(r) => mkdir(id, &r.path, &r.attrs),
            Packet::RmDir(r) => fs::remove_dir(&r.path).map(|()| ok(id)).map_err(Into::into),
            Packet::RealPath(r) => realpath(id, &r.path),
            Packet::Rename(r) => rename(id, &r.oldpath, &r.newpath),
            Packet::ReadLink(r) => readlink(id, &r.path),
            // OpenSSH's client puts the link TARGET first and the new link's
            // path second, the reverse of the draft, and OpenSSH's server
            // follows its client. russh-sftp names the fields in draft order,
            // so `linkpath` here carries the target.
            Packet::Symlink(r) => std::os::unix::fs::symlink(&r.linkpath, &r.targetpath)
                .map(|()| ok(id))
                .map_err(Into::into),
            Packet::Extended(_) => Err(Failure::new(StatusCode::OpUnsupported)),
            // A reply type sent as a request.
            _ => Err(Failure::new(StatusCode::BadMessage)),
        };
        match reply {
            Ok(packet) => packet,
            Err(failure) => Packet::status(id, failure.code, &failure.message, "en-US"),
        }
    }

    fn insert(&mut self, handle: Handle) -> String {
        let key = self.next_handle.to_string();
        self.next_handle += 1;
        self.handles.insert(key.clone(), handle);
        key
    }

    fn file(&self, handle: &str) -> Result<&File, Failure> {
        match self.handles.get(handle) {
            Some(Handle::File(file)) => Ok(file),
            _ => Err(Failure::new(StatusCode::Failure)),
        }
    }

    fn open(&mut self, id: u32, path: &str, flags: OpenFlags, attrs: &FileAttributes) -> Reply {
        let write = flags.intersects(OpenFlags::WRITE | OpenFlags::APPEND);
        let mut options = OpenOptions::new();
        // Neither READ nor WRITE is O_RDONLY, as in OpenSSH's sftp-server.
        options
            .read(flags.contains(OpenFlags::READ) || !write)
            .write(flags.contains(OpenFlags::WRITE))
            .append(flags.contains(OpenFlags::APPEND))
            .truncate(flags.contains(OpenFlags::TRUNCATE))
            .mode(attrs.permissions.map_or(0o666, |mode| mode & 0o7777));
        if flags.contains(OpenFlags::CREATE) {
            if flags.contains(OpenFlags::EXCLUDE) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        let file = options.open(path)?;
        let handle = self.insert(Handle::File(file));
        Ok(Packet::Handle(HandleReply { id, handle }))
    }

    fn close(&mut self, id: u32, handle: &str) -> Reply {
        match self.handles.remove(handle) {
            Some(_) => Ok(ok(id)),
            None => Err(Failure::new(StatusCode::Failure)),
        }
    }

    fn read(&self, id: u32, handle: &str, offset: u64, len: u32) -> Reply {
        let file = self.file(handle)?;
        let mut data = vec![0u8; len.min(MAX_READ_LENGTH) as usize];
        let mut filled = 0;
        while filled < data.len() {
            match file.read_at(&mut data[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }
        }
        if filled == 0 && !data.is_empty() {
            return Err(Failure::new(StatusCode::Eof));
        }
        data.truncate(filled);
        Ok(Packet::Data(Data { id, data }))
    }

    fn write(&self, id: u32, handle: &str, offset: u64, data: &[u8]) -> Reply {
        self.file(handle)?.write_all_at(data, offset)?;
        Ok(ok(id))
    }

    fn fstat(&self, id: u32, handle: &str) -> Reply {
        match self.handles.get(handle) {
            Some(Handle::File(file)) => Ok(attrs(id, &file.metadata()?)),
            Some(Handle::Dir { path, .. }) => Ok(attrs(id, &fs::metadata(path)?)),
            None => Err(Failure::new(StatusCode::Failure)),
        }
    }

    fn fsetstat(&self, id: u32, handle: &str, attrs: &FileAttributes) -> Reply {
        match self.handles.get(handle) {
            Some(Handle::File(file)) => set_file_attributes(file, attrs)?,
            Some(Handle::Dir { path, .. }) => set_path_attributes(path, attrs)?,
            None => return Err(Failure::new(StatusCode::Failure)),
        }
        Ok(ok(id))
    }

    fn opendir(&mut self, id: u32, path: &str) -> Reply {
        let entries = fs::read_dir(path)?;
        let handle = self.insert(Handle::Dir {
            path: PathBuf::from(path),
            entries,
        });
        Ok(Packet::Handle(HandleReply { id, handle }))
    }

    fn readdir(&mut self, id: u32, handle: &str) -> Reply {
        let Some(Handle::Dir { entries, .. }) = self.handles.get_mut(handle) else {
            return Err(Failure::new(StatusCode::Failure));
        };
        let now = chrono::Local::now().timestamp();
        let mut files = Vec::new();
        for entry in entries.by_ref() {
            let entry = entry?;
            // An entry removed between the listing and its lstat is skipped,
            // as OpenSSH's sftp-server does.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let filename = entry.file_name().to_string_lossy().into_owned();
            files.push(NameEntry {
                longname: long_name(&filename, &meta, now),
                filename,
                attrs: attributes(&meta),
            });
            if files.len() == READDIR_BATCH {
                break;
            }
        }
        if files.is_empty() {
            return Err(Failure::new(StatusCode::Eof));
        }
        Ok(Packet::Name(Name { id, files }))
    }
}

/// The request id of a body that may not decode: bytes 1..5, except for
/// `SSH_FXP_INIT`, which has none.
fn raw_request_id(body: &[u8]) -> u32 {
    match body {
        [kind, a, b, c, d, ..] if *kind != SSH_FXP_INIT => u32::from_be_bytes([*a, *b, *c, *d]),
        _ => 0,
    }
}

/// Whether `kind` is a request type of SFTP v3: `SSH_FXP_INIT`, `SSH_FXP_OPEN`
/// (3) through `SSH_FXP_SYMLINK` (20), or `SSH_FXP_EXTENDED`. Anything else is
/// answered `SSH_FX_OP_UNSUPPORTED` under its request id, as OpenSSH does.
fn is_request_type(kind: u8) -> bool {
    kind == SSH_FXP_INIT || (3..=20).contains(&kind) || kind == SSH_FXP_EXTENDED
}

fn ok(id: u32) -> Packet {
    Packet::Status(Status {
        id,
        status_code: StatusCode::Ok,
        error_message: StatusCode::Ok.to_string(),
        language_tag: "en-US".to_string(),
    })
}

fn attrs(id: u32, meta: &fs::Metadata) -> Packet {
    Packet::Attrs(Attrs {
        id,
        attrs: attributes(meta),
    })
}

/// The attributes of `meta`, with the full `st_mode` as permissions.
///
/// Not russh-sftp's `From<&Metadata>`, which ORs the regular-file bit into
/// every non-directory and clears the directory bit from every other type, so
/// a character device reads back as a symlink.
fn attributes(meta: &fs::Metadata) -> FileAttributes {
    FileAttributes {
        size: Some(meta.size()),
        uid: Some(meta.uid()),
        user: None,
        gid: Some(meta.gid()),
        group: None,
        permissions: Some(meta.mode()),
        atime: Some(wire_time(meta.atime())),
        mtime: Some(wire_time(meta.mtime())),
    }
}

/// SFTP v3 carries times as unsigned 32-bit seconds.
fn wire_time(seconds: i64) -> u32 {
    u32::try_from(seconds.max(0)).unwrap_or(u32::MAX)
}

/// An `ls -l` line in the shape OpenSSH's sftp-server sends, with numeric
/// owner and group: mode, links, uid, gid, size, date, name. The date shows
/// the time for the last six months and the year otherwise.
fn long_name(name: &str, meta: &fs::Metadata, now: i64) -> String {
    const SIX_MONTHS: i64 = 365 * 24 * 60 * 60 / 2;
    let mtime = meta.mtime();
    let date = chrono::DateTime::from_timestamp(mtime, 0)
        .map(|utc| utc.with_timezone(&chrono::Local))
        .map_or_else(String::new, |local| {
            if (now - mtime).abs() < SIX_MONTHS {
                local.format("%b %e %H:%M").to_string()
            } else {
                local.format("%b %e  %Y").to_string()
            }
        });
    format!(
        "{:<10} {:>3} {:<8} {:<8} {:>8} {date} {name}",
        mode_string(meta.mode()),
        meta.nlink(),
        meta.uid(),
        meta.gid(),
        meta.size(),
    )
}

/// `strmode(3)`: the type character and the nine permission characters.
fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let bit = |mask: u32, c: char| if mode & mask != 0 { c } else { '-' };
    let special = |exec: u32, special: u32, set: char, unset: char| match (
        mode & exec != 0,
        mode & special != 0,
    ) {
        (true, true) => set,
        (false, true) => unset,
        (true, false) => 'x',
        (false, false) => '-',
    };
    [
        kind,
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        special(0o100, 0o4000, 's', 'S'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        special(0o010, 0o2000, 's', 'S'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        special(0o001, 0o1000, 't', 'T'),
    ]
    .iter()
    .collect()
}

/// Apply `attrs` to a path, in OpenSSH's order: size, permissions, times,
/// owner. Symlinks are followed, as `chmod`, `truncate` and `utimes` do.
fn set_path_attributes(path: &Path, attrs: &FileAttributes) -> Result<(), Failure> {
    if let Some(size) = attrs.size {
        OpenOptions::new().write(true).open(path)?.set_len(size)?;
    }
    if let Some(mode) = attrs.permissions {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let (Some(atime), Some(mtime)) = (attrs.atime, attrs.mtime) {
        let timespec = |seconds: u32| rustix::fs::Timespec {
            tv_sec: i64::from(seconds),
            tv_nsec: 0,
        };
        rustix::fs::utimensat(
            rustix::fs::CWD,
            path,
            &rustix::fs::Timestamps {
                last_access: timespec(atime),
                last_modification: timespec(mtime),
            },
            rustix::fs::AtFlags::empty(),
        )
        .map_err(io::Error::from)?;
    }
    if attrs.uid.is_some() || attrs.gid.is_some() {
        std::os::unix::fs::chown(path, attrs.uid, attrs.gid)?;
    }
    Ok(())
}

/// [`set_path_attributes`] through an open file.
fn set_file_attributes(file: &File, attrs: &FileAttributes) -> Result<(), Failure> {
    if let Some(size) = attrs.size {
        file.set_len(size)?;
    }
    if let Some(mode) = attrs.permissions {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let (Some(atime), Some(mtime)) = (attrs.atime, attrs.mtime) {
        let time = |seconds: u32| UNIX_EPOCH + Duration::from_secs(u64::from(seconds));
        file.set_times(
            FileTimes::new()
                .set_accessed(time(atime))
                .set_modified(time(mtime)),
        )?;
    }
    if attrs.uid.is_some() || attrs.gid.is_some() {
        std::os::unix::fs::fchown(file, attrs.uid, attrs.gid)?;
    }
    Ok(())
}

fn mkdir(id: u32, path: &str, attrs: &FileAttributes) -> Reply {
    DirBuilder::new()
        .mode(attrs.permissions.map_or(0o777, |mode| mode & 0o7777))
        .create(path)?;
    Ok(ok(id))
}

/// SFTP v3 rename, which fails when `newpath` exists rather than replacing it
/// (OpenSSH's sftp-server does the same without `posix-rename@openssh.com`,
/// which is not advertised). The check and the rename are two steps; a name
/// created between them is replaced.
fn rename(id: u32, oldpath: &str, newpath: &str) -> Reply {
    fs::symlink_metadata(oldpath)?;
    if fs::symlink_metadata(newpath).is_ok() {
        return Err(Failure {
            code: StatusCode::Failure,
            message: format!("{newpath} already exists"),
        });
    }
    fs::rename(oldpath, newpath)?;
    Ok(ok(id))
}

fn single_name(id: u32, name: String) -> Packet {
    Packet::Name(Name {
        id,
        files: vec![NameEntry {
            longname: name.clone(),
            filename: name,
            attrs: FileAttributes::default(),
        }],
    })
}

fn readlink(id: u32, path: &str) -> Reply {
    let target = fs::read_link(path)?;
    Ok(single_name(id, target.to_string_lossy().into_owned()))
}

/// The canonical absolute form of `path`, relative ones taken from the
/// working directory (the account's home, where the SSH server starts this
/// process). The last component may be missing, so a client can resolve the
/// name it is about to create; a missing directory above it is
/// `SSH_FX_NO_SUCH_FILE`.
fn realpath(id: u32, path: &str) -> Reply {
    let path = if path.is_empty() { "." } else { path };
    let resolved = match fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let absolute = std::env::current_dir()?.join(path);
            let mut components = absolute.components();
            match components.next_back() {
                Some(Component::Normal(last)) => fs::canonicalize(components.as_path())?.join(last),
                _ => return Err(err.into()),
            }
        }
        Err(err) => return Err(err.into()),
    };
    Ok(single_name(id, resolved.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests;
