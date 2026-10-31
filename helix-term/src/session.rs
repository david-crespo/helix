//! Sharing one editor between terminals.
//!
//! With sessions on, the first `hx` in a session starts a server that runs the editor, and every
//! `hx` becomes a thin client: it hands its terminal to the server over a Unix socket and waits
//! for the server to say it's done. The server draws to the terminal and reads its input
//! directly, so nothing else goes over the socket but a few control messages.
//!
//! The protocol: the client sends one byte carrying its terminal's file descriptor, and its
//! stdin's if stdin is piped, as `SCM_RIGHTS`. A `--remote` client sends none, since its files
//! open in another client's terminal. Then it sends a [`Hello`]. After that the client
//! sends [`ClientMessage`]s and the server [`ServerMessage`]s. Each message is a little-endian
//! `u32` length followed by that many bytes of JSON.

// Integration tests swap the terminal for a test backend, so they can't serve sessions.
#![cfg_attr(feature = "integration", allow(dead_code))]

use std::{
    fs::File,
    io::{self, IoSlice, IoSliceMut, IsTerminal as _, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::{
            fs::{DirBuilderExt as _, MetadataExt as _},
            net::UnixStream,
            process::CommandExt as _,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use anyhow::Context as _;
use helix_core::Position;
use helix_view::{editor::SessionConfig, tree::Layout, ClientId};
use indexmap::IndexMap;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{args::Args, config::Config};

/// Bumped whenever a message changes shape, so a client never talks to a server it can't
/// understand.
const PROTOCOL_VERSION: u32 = 2;

/// Tells builds of `hx` apart: the modification time and size of this process's binary, as
/// they were when this was first called. A server calls it as it starts, since the binary is
/// replaced under it when `hx` is rebuilt.
pub fn build_id() -> &'static str {
    static BUILD: OnceLock<String> = OnceLock::new();
    BUILD.get_or_init(|| {
        let meta = std::env::current_exe().and_then(std::fs::metadata);
        meta.map(|meta| format!("{}.{}-{}", meta.mtime(), meta.mtime_nsec(), meta.size()))
            .unwrap_or_default()
    })
}

static SERVER: AtomicBool = AtomicBool::new(false);

/// Whether this process is a session server. A server has no terminal of its own, so things
/// like suspending apply to a client instead.
pub fn is_server() -> bool {
    SERVER.load(Ordering::Relaxed)
}

pub fn set_server() {
    SERVER.store(true, Ordering::Relaxed);
}

static SUSPEND_REQUESTS: Mutex<Vec<ClientId>> = Mutex::new(Vec::new());

/// Asks the server to give `client`'s terminal back to its shell, as `C-z` would standalone.
/// Not a signal, which would arrive too late to tell which client asked.
pub fn request_suspend(client: ClientId) {
    SUSPEND_REQUESTS.lock().unwrap().push(client);
}

pub fn take_suspend_requests() -> Vec<ClientId> {
    std::mem::take(&mut SUSPEND_REQUESTS.lock().unwrap())
}

/// What a client tells the server when it connects: where it runs, and what its command line
/// asks for. The client parses the command line, so paths here are absolute.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    /// The client's [`build_id`].
    pub build: String,
    /// The files to open, with the positions to put cursors at.
    pub files: IndexMap<PathBuf, Vec<Position>>,
    pub split: Option<Layout>,
    pub load_tutor: bool,
    /// With `--remote`, whether to wait until none of the files is shown.
    pub wait: bool,
    /// The client's working directory: the one `--working-dir` or a directory argument asks
    /// for, or else where it was started.
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Whether a second file descriptor, the client's stdin, came with the terminal's.
    pub stdin_piped: bool,
}

impl Hello {
    fn new(args: &Args, stdin_piped: bool) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            build: build_id().to_owned(),
            files: args.files.clone(),
            split: args.split,
            load_tutor: args.load_tutor,
            wait: args.wait,
            cwd: helix_stdx::env::current_working_dir(),
            env: std::env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
            stdin_piped,
        }
    }

    pub fn var(&self, name: &str) -> Option<String> {
        self.env
            .iter()
            .find_map(|(key, value)| (key == name).then(|| value.clone()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// The terminal changed size. The server reads the new size from the terminal itself.
    Resized,
    /// The client was suspended and is now back in the foreground.
    Continued,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// The server has given the terminal back. The client should stop itself, as if the user
    /// had pressed `C-z` in a shell, and send [`ClientMessage::Continued`] once it's resumed.
    Suspend,
    /// Something the user should see, which the client prints to its terminal. Sent while
    /// the server isn't drawing to it: before it's set up, or once it's restored.
    Error(String),
    /// The client's editor is done. The server has restored the terminal.
    Exit(i32),
}

pub fn write_message(mut writer: impl Write, message: &impl Serialize) -> io::Result<()> {
    writer.write_all(&frame(message)?)
}

/// `message` with its length in front, as it goes over the socket.
pub fn frame(message: &impl Serialize) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(message)?;
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    let mut frame = len.to_le_bytes().to_vec();
    frame.extend(body);
    Ok(frame)
}

pub fn read_message<T: DeserializeOwned>(mut reader: impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let mut body = vec![0; body_len(len)?];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

pub async fn read_message_async<T: DeserializeOwned>(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> io::Result<T> {
    use tokio::io::AsyncReadExt as _;
    let mut len = [0; 4];
    reader.read_exact(&mut len).await?;
    let mut body = vec![0; body_len(len)?];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

fn body_len(len: [u8; 4]) -> io::Result<usize> {
    // Far more than any message needs, to catch a peer that isn't speaking this protocol.
    const MAX_LEN: u32 = 1 << 20;
    let len = u32::from_le_bytes(len);
    if len > MAX_LEN {
        return Err(io::Error::other(format!(
            "a {len} byte message is too long"
        )));
    }
    Ok(len as usize)
}

/// The session `hx` should join, if any.
///
/// `--session` wins. Otherwise `hx` joins a session unless `--no-session` or
/// `editor.session = "off"` say not to, or `--config` names a config, which an editor already
/// running wouldn't use. The key is `$HELIX_SESSION` or else derived from the workspace, so every
/// `hx` in a workspace shares one editor.
pub fn session_key(args: &Args, config: &Config) -> Option<String> {
    if let Some(name) = &args.session {
        return Some(sanitize(name));
    }
    if args.no_session || args.config_file.is_some() || config.editor.session == SessionConfig::Off
    {
        return None;
    }
    if let Some(name) = std::env::var("HELIX_SESSION")
        .ok()
        .filter(|n| !n.is_empty())
    {
        return Some(sanitize(&name));
    }
    let workspace = helix_loader::find_workspace().0;
    let name = workspace
        .file_name()
        .map(|name| sanitize(&name.to_string_lossy()))
        .unwrap_or_default();
    // Short enough to keep the socket path within `sun_path`'s ~100 bytes.
    Some(format!(
        "{}-{:012x}",
        name.chars().take(12).collect::<String>(),
        fnv1a(workspace.as_os_str().as_encoded_bytes()) & 0xffff_ffff_ffff
    ))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// A hash that stays the same across builds, unlike `std`'s.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// `$XDG_RUNTIME_DIR/helix/<key>.sock`, or `$TMPDIR/helix-$UID/<key>.sock`. The directory must
/// be private to this user.
pub fn socket_path(key: &str) -> io::Result<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("helix"),
        None => std::env::temp_dir().join(format!("helix-{uid}")),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    // Not followed if it's a symlink, which could lead to a directory someone else can write.
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} must be a directory that belongs to you and is private (mode 0700)",
            dir.display()
        )));
    }
    let path = dir.join(format!("{key}.sock"));
    // A socket's path has to fit in `sun_path`, about 100 bytes.
    if rustix::net::SocketAddrUnix::new(&path).is_err() {
        return Err(io::Error::other(format!(
            "the session name is too long: {} is more than a socket's path can be",
            path.display()
        )));
    }
    Ok(path)
}

fn log_path(key: &str) -> PathBuf {
    helix_loader::cache_dir().join(format!("session-{key}.log"))
}

/// Runs `hx` as a client of the session `key`'s server, starting the server if needed, and
/// returns the exit code for this process.
pub fn run_client(key: &str, args: &Args) -> anyhow::Result<i32> {
    // `pkill -USR1 hx`, which has Helix reload its config, reaches clients too. Taking the
    // signal here keeps it from ending them: the server does the reloading.
    use signal_hook::consts::{SIGUSR1, SIGWINCH};
    let mut signals = signal_hook::iterator::Signals::new([SIGWINCH, SIGUSR1])?;

    let path = socket_path(key)?;
    let conn = match UnixStream::connect(&path) {
        Ok(conn) if args.remote => return run_remote(&conn, args),
        Ok(conn) => conn,
        // With no session to open them in, `--remote` files open here.
        Err(_) => {
            let server = spawn_server(key, args)?;
            wait_for_server(&path, key, server)?
        }
    };

    // Not `/dev/tty`: on macOS that stands for whatever terminal the process using it controls,
    // and the server controls none.
    let (stdin, stdout, stderr) = (io::stdin(), io::stdout(), io::stderr());
    let tty = [stdout.as_fd(), stdin.as_fd(), stderr.as_fd()]
        .into_iter()
        .find(|fd| rustix::termios::isatty(fd))
        .context("helix needs a terminal")?;
    // Restored when the server is done, however it ends, so a crash never leaves the terminal
    // in raw mode.
    let termios = rustix::termios::tcgetattr(tty)?;
    let stdin_piped = !stdin.is_terminal();
    let mut fds = vec![tty];
    if stdin_piped {
        fds.push(stdin.as_fd());
    }
    send_fds(&conn, &fds)?;
    write_message(&conn, &Hello::new(args, stdin_piped))?;

    let resized = conn.try_clone()?;
    std::thread::spawn(move || {
        for _ in signals.forever().filter(|&signal| signal == SIGWINCH) {
            if write_message(&resized, &ClientMessage::Resized).is_err() {
                break;
            }
        }
    });

    let restore = || {
        let _ = rustix::termios::tcsetattr(tty, rustix::termios::OptionalActions::Now, &termios);
    };
    loop {
        match read_message(&conn) {
            Ok(ServerMessage::Suspend) => {
                // Stop the whole process group, so that a program waiting on `hx`, like
                // `git commit`, stops too and the shell gets the terminal back. This returns
                // once we're continued, or right away if the group can't be stopped.
                let _ = rustix::process::kill_current_process_group(rustix::process::Signal::TSTP);
                write_message(&conn, &ClientMessage::Continued)?;
            }
            Ok(ServerMessage::Error(message)) => eprintln!("{message}"),
            Ok(ServerMessage::Exit(code)) => {
                restore();
                return Ok(code);
            }
            Err(err) => {
                // Undo what the server may have set up: the alternate screen, mouse reporting,
                // bracketed paste, focus events and the kitty keyboard protocol.
                let _ = rustix::io::write(
                    tty,
                    b"\x1b[<u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?25h\x1b[?1049l",
                );
                restore();
                eprintln!(
                    "The helix session server went away ({err}). Its log is {}",
                    log_path(key).display()
                );
                return Ok(1);
            }
        }
    }
}

/// Has the server open this command line's files in its focused client, and waits for it to
/// say it's done.
fn run_remote(conn: &UnixStream, args: &Args) -> anyhow::Result<i32> {
    send_fds(conn, &[])?;
    write_message(conn, &Hello::new(args, false))?;
    loop {
        match read_message(conn).context("the helix session server went away")? {
            ServerMessage::Error(message) => eprintln!("{message}"),
            ServerMessage::Exit(code) => return Ok(code),
            ServerMessage::Suspend => {}
        }
    }
}

/// Starts the server for `key` in its own session, so it outlives this terminal.
fn spawn_server(key: &str, args: &Args) -> io::Result<Child> {
    let log_path = log_path(key);
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = File::options().create(true).append(true).open(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("--session-server").arg(key);
    // Absolute, since the server leaves this directory.
    if let Some(config) = &args.config_file {
        command
            .arg("--config")
            .arg(helix_stdx::path::canonicalize(config));
    }
    if let Some(log_file) = &args.log_file {
        command
            .arg("--log")
            .arg(helix_stdx::path::canonicalize(log_file));
    }
    for _ in 0..args.verbosity {
        command.arg("-v");
    }
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: `setsid` is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
    command.spawn()
}

/// Connects to the server just started as `server`, once it listens.
fn wait_for_server(path: &Path, key: &str, mut server: Child) -> anyhow::Result<UnixStream> {
    let start = Instant::now();
    loop {
        let err = match UnixStream::connect(path) {
            Ok(conn) => return Ok(conn),
            Err(err) => err,
        };
        // A server that exits cleanly found another one starting at the same moment, which
        // will listen soon. One that failed won't.
        let failed = server.try_wait()?.filter(|status| !status.success());
        if failed.is_none() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let reason = failed.map_or(err.to_string(), |status| format!("it failed ({status})"));
        anyhow::bail!(
            "couldn't reach the helix session server at {}: {reason}. Its log is {}",
            path.display(),
            log_path(key).display()
        );
    }
}

/// A client that has connected and introduced itself.
pub struct Attach {
    pub hello: Hello,
    /// The client's terminal, unless it's a `--remote` client.
    pub tty: Option<OwnedFd>,
    /// Everything piped to the client's stdin.
    pub stdin: Option<Vec<u8>>,
}

/// Receives a client's terminal and [`Hello`]. Blocks until the client has sent them and, if
/// stdin is piped, until it's closed.
pub fn handshake(conn: &UnixStream) -> io::Result<Attach> {
    let mut fds = recv_fds(conn)?.into_iter();
    let hello: Hello = read_message(conn)?;
    if hello.version != PROTOCOL_VERSION {
        return Err(io::Error::other(format!(
            "this hx speaks protocol version {}, and the session's server, another build, \
             version {PROTOCOL_VERSION}. Quit every hx in the session to restart it, or pass \
             --no-session",
            hello.version
        )));
    }
    if hello.build != build_id() {
        // Sent before the terminal is set up, so the client prints it above the editor.
        let warning = "This session's server is a different build of hx, probably an older one. \
            To restart it, quit every hx in the session (:session-quit), then start hx again.";
        write_message(conn, &ServerMessage::Error(warning.into()))?;
    }
    let tty = fds.next();
    let stdin = match (hello.stdin_piped, fds.next()) {
        (true, Some(fd)) => {
            let mut contents = Vec::new();
            File::from(fd).read_to_end(&mut contents)?;
            Some(contents)
        }
        _ => None,
    };
    Ok(Attach { hello, tty, stdin })
}

fn send_fds(conn: &UnixStream, fds: &[BorrowedFd]) -> io::Result<()> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() {
        control.push(SendAncillaryMessage::ScmRights(fds));
    }
    rustix::net::sendmsg(
        conn,
        &[IoSlice::new(&[0])],
        &mut control,
        SendFlags::empty(),
    )?;
    Ok(())
}

fn recv_fds(conn: &UnixStream) -> io::Result<Vec<OwnedFd>> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut byte = [0];
    let received = rustix::net::recvmsg(
        conn,
        &mut [IoSliceMut::new(&mut byte)],
        &mut control,
        RecvFlags::empty(),
    )?;
    if received.bytes == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut fds = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = message {
            fds.extend(received);
        }
    }
    // Keep the client's terminal out of the language servers and shells we start.
    for fd in &fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    Ok(fds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_on_unless_turned_off() {
        let key = |args: Args, session| {
            let mut config = Config::default();
            if let Some(session) = session {
                config.editor.session = session;
            }
            session_key(&args, &config)
        };
        assert!(key(Args::default(), None).is_some());
        assert!(key(Args::default(), Some(SessionConfig::Off)).is_none());
        let no_session = Args {
            no_session: true,
            ..Args::default()
        };
        assert!(key(no_session, None).is_none());
        // A shared editor runs with the config it started with, so it can't use this one.
        let other_config = Args {
            config_file: Some("other.toml".into()),
            ..Args::default()
        };
        assert!(key(other_config, None).is_none());
        let named = Args {
            session: Some("named".into()),
            config_file: Some("other.toml".into()),
            ..Args::default()
        };
        assert_eq!(
            key(named, Some(SessionConfig::Off)).as_deref(),
            Some("named")
        );
    }
}
