//! The server side of [`crate::session`]: one editor shared between the terminals of every
//! `hx` that connects. Each connection becomes a client with its own layout, compositor and
//! terminal, which the server reads and draws to directly.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use arc_swap::ArcSwap;
use futures_util::{future, stream, StreamExt as _};
use helix_core::{pos_at_coords, syntax, Range};
use helix_loader::workspace_trust::TrustQuery;
use helix_view::{
    align_view, current, doc_mut,
    editor::{Action, ClientState},
    view_mut, Align, ClientId, DocumentId, Editor,
};
use termina::{PlatformTerminal, Terminal as _};
use tokio::{
    io::AsyncWriteExt as _,
    net::unix::OwnedReadHalf,
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tokio_stream::StreamMap;
use tui::backend::{Backend as _, TerminalWriter};

use super::{Application, ClientUi, Terminal, TerminalBackend, TerminalEvent};
use crate::{
    args::Args,
    commands::ScriptingEngine,
    compositor::Compositor,
    config::Config,
    events::{ClientAttached, ClientDetached},
    handlers,
    session::{self, ClientMessage, Hello, ServerMessage},
};

pub struct Server {
    listener: tokio::net::UnixListener,
    socket: Socket,
    config: Arc<ArcSwap<Config>>,
    attached: Channel<(UnixStream, anyhow::Result<Box<NewClient>>)>,
    messages: Channel<(ClientId, Option<ClientMessage>)>,
    inputs: StreamMap<ClientId, termina::EventStream>,
    conns: HashMap<ClientId, Conn>,
    suspended: HashSet<ClientId>,
    waits: Vec<Wait>,
    next_client: u32,
}

/// A `--remote --wait` command line, waiting until none of the files it opened is shown.
struct Wait {
    conn: Conn,
    /// The client they opened in, whose exit code the command line exits with.
    client: ClientId,
    docs: Vec<DocumentId>,
    code: i32,
}

type Channel<T> = (UnboundedSender<T>, UnboundedReceiver<T>);

/// A connection to an `hx` process. Messages to it go out through a task, so a process that
/// stops reading can't stall the server.
pub struct Conn {
    messages: UnboundedSender<ServerMessage>,
    writer: JoinHandle<()>,
    /// Taken by [`Server::add`] to read what a client sends.
    read: Option<OwnedReadHalf>,
}

impl Conn {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        let (read, mut write) = tokio::net::UnixStream::from_std(stream)?.into_split();
        let (messages, mut queue) = unbounded_channel::<ServerMessage>();
        let writer = helix_event::client::spawn(async move {
            while let Some(message) = queue.recv().await {
                let sent = match session::frame(&message) {
                    Ok(frame) => write.write_all(&frame).await,
                    Err(err) => Err(err),
                };
                if let Err(err) = sent {
                    log::warn!("couldn't send {message:?}: {err}");
                    break;
                }
            }
        });
        Ok(Self {
            messages,
            writer,
            read: Some(read),
        })
    }

    fn send(&self, message: ServerMessage) {
        let _ = self.messages.send(message);
    }

    /// Sends `messages` once what was drawn to `terminal` has reached it, so the client doesn't
    /// take its terminal back before it's been reset.
    fn send_after(&self, terminal: &mut Terminal, messages: Vec<ServerMessage>) {
        let sender = self.messages.clone();
        terminal.backend_mut().then(move || {
            for message in messages {
                let _ = sender.send(message);
            }
        });
    }

    /// Waits for what was sent to go out, for at most a second.
    async fn close(self) {
        drop(self.messages);
        let _ = tokio::time::timeout(Duration::from_secs(1), self.writer).await;
    }
}

/// A client that has handed over its terminal, ready to join, or a `--remote` client.
pub struct NewClient {
    hello: Hello,
    stdin: Option<Vec<u8>>,
    /// `None` for a `--remote` client.
    backend: Option<TerminalBackend>,
    /// Another handle on the client's terminal, for clipboard copies sent as OSC 52.
    clipboard: Option<Box<dyn Write + Send>>,
}

pub enum ServerEvent {
    Attached(Conn, anyhow::Result<Box<NewClient>>),
    Input(ClientId, io::Result<TerminalEvent>),
    /// A message from a client, or `None` when its connection closed.
    Message(ClientId, Option<ClientMessage>),
}

pub async fn next_event(server: &mut Option<Server>) -> ServerEvent {
    match server {
        Some(server) => server.next_event().await,
        None => future::pending().await,
    }
}

/// Runs the session server for `key` until its last client leaves, and returns the exit code
/// for this process.
pub async fn serve(
    key: &str,
    config: Config,
    lang_loader: syntax::Loader,
    workspace_trust: helix_loader::workspace_trust::WorkspaceTrust,
) -> anyhow::Result<i32> {
    session::set_server();
    helix_stdx::env::keep_process_working_dir();
    let path = session::socket_path(key)?;
    let Some((listener, socket)) = bind(&path)? else {
        log::info!("another server is already serving {}", path.display());
        return Ok(0);
    };
    log::info!("serving session {key} on {}", path.display());

    // The first client with a terminal starts the editor. `--remote` command lines that get
    // here before it wait for it.
    let mut remotes = Vec::new();
    let (conn, client) = loop {
        let (stream, _) = listener.accept()?;
        let client = attach_terminal(&stream, &config);
        let conn = Conn::new(stream)?;
        match client {
            Ok(client) if client.backend.is_none() => remotes.push((conn, client.hello)),
            client => break (conn, client),
        }
    };
    let app = client.and_then(|client| {
        let backend = client.backend.expect("a client with a terminal");
        helix_stdx::env::swap_environment(Some(environment(&client.hello)));
        helix_view::clipboard::swap_terminal(client.clipboard);
        let args = client_args(&client.hello)?;
        let auto_save = args.auto_save;
        let mut stdin = client.stdin.as_deref();
        let mut app = Application::with_backend(
            args,
            config,
            lang_loader,
            workspace_trust,
            backend,
            stdin.as_mut().map(|stdin| stdin as &mut dyn Read),
        )?;
        app.editor.registers.client_clipboard = Some(client.hello.clipboard_provider);
        if let Some(delay) = auto_save {
            app.editor
                .client_auto_save
                .insert(ClientId::default(), delay);
        }
        Ok(app)
    });
    let mut app = match app {
        Ok(app) => app,
        Err(err) => {
            for conn in remotes.into_iter().map(|(conn, _)| conn).chain([conn]) {
                refuse(&conn, &err);
                conn.close().await;
            }
            return Err(err);
        }
    };
    app.terminal.claim()?;

    listener.set_nonblocking(true)?;
    let mut server = Server {
        listener: tokio::net::UnixListener::from_std(listener)?,
        socket,
        config: app.config.clone(),
        attached: unbounded_channel(),
        messages: unbounded_channel(),
        inputs: StreamMap::new(),
        conns: HashMap::new(),
        suspended: HashSet::new(),
        waits: Vec::new(),
        next_client: 1,
    };
    server.add(ClientId::default(), conn, app.event_stream());
    app.server = Some(server);
    for (conn, hello) in remotes {
        app.open_remote(conn, &hello);
    }

    app.event_loop(&mut stream::pending()).await;

    // Stop taking new clients first, so they start a new server rather than join this one as
    // it shuts down.
    let Server {
        listener,
        socket,
        mut conns,
        mut waits,
        ..
    } = app.server.take().expect("still serving");
    drop((socket, listener));
    let errors = app.close().await;
    ScriptingEngine::shutdown();
    let exit = |code| {
        let mut messages: Vec<_> = errors
            .iter()
            .map(|err| ServerMessage::Error(format!("Error: {err}")))
            .collect();
        messages.push(ServerMessage::Exit(if errors.is_empty() {
            code
        } else {
            1
        }));
        messages
    };
    let mut closing = Vec::new();
    for client in app.editor.clients().collect::<Vec<_>>() {
        app.activate(client);
        let _ = app.restore_term();
        let code = app.editor.exit_code;
        for wait in waits.iter_mut().filter(|wait| wait.client == client) {
            wait.code = code;
        }
        if let Some(conn) = conns.remove(&client) {
            conn.send_after(&mut app.terminal, exit(code));
            closing.push(conn);
        }
    }
    for wait in waits {
        exit(wait.code)
            .into_iter()
            .for_each(|message| wait.conn.send(message));
        closing.push(wait.conn);
    }
    drop(app);
    future::join_all(closing.into_iter().map(Conn::close)).await;
    Ok(0)
}

/// The session's socket, which this server alone listens on until it's dropped.
struct Socket {
    path: PathBuf,
    /// Held while the socket is ours, so that two servers starting at once can't both take it.
    _lock: File,
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listens on `path`, unless another server already does.
fn bind(path: &Path) -> io::Result<Option<(UnixListener, Socket)>> {
    let lock = File::create(path.with_extension("lock"))?;
    match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
        locked => locked?,
    }
    // Left behind by a server that crashed.
    if let Err(err) = std::fs::remove_file(path) {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
    }
    let listener = UnixListener::bind(path)?;
    let socket = Socket {
        path: path.to_owned(),
        _lock: lock,
    };
    Ok(Some((listener, socket)))
}

/// Receives a client's terminal and sets it up. This blocks on the client, and on its terminal
/// answering queries.
fn attach_terminal(conn: &UnixStream, config: &Config) -> anyhow::Result<NewClient> {
    let attach = session::handshake(conn).context("handshake failed")?;
    let Some(tty) = attach.tty else {
        return Ok(NewClient {
            hello: attach.hello,
            stdin: attach.stdin,
            backend: None,
            clipboard: None,
        });
    };
    let writer = TerminalWriter::new(tty.try_clone()?.into(), helix_event::request_redraw)?;
    let terminal = PlatformTerminal::from_fd(tty).context("can't use the terminal")?;
    let mut backend = TerminalBackend::with_terminal(terminal, (&config.editor).into(), &|name| {
        attach.hello.var(name)
    })
    .context("can't set up the terminal")?;
    backend.set_writer(writer.clone());
    Ok(NewClient {
        hello: attach.hello,
        stdin: attach.stdin,
        backend: Some(backend),
        clipboard: Some(Box::new(writer)),
    })
}

/// Parses a client's command line from its own working directory, then moves into the one it
/// asks for.
fn client_args(hello: &Hello) -> anyhow::Result<Args> {
    helix_stdx::env::set_current_working_dir(&hello.cwd)?;
    let args = Args::parse(hello.argv.iter().cloned())?;
    let dir = args.working_directory.as_ref().or_else(|| {
        let (first, _) = args.files.first()?;
        first.is_dir().then_some(first)
    });
    if let Some(dir) = dir {
        helix_stdx::env::set_current_working_dir(dir)?;
    }
    Ok(args)
}

fn environment(hello: &Hello) -> helix_stdx::env::Environment {
    hello
        .env
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

/// Reloads the unmodified document open at `path` if the file changed since it was read, so a
/// client opening it sees what a new editor would. Git rewrites `COMMIT_EDITMSG` like this
/// between commits.
fn reload_if_changed(editor: &mut Editor, path: &Path) {
    let Some(doc) = editor.document_by_path(path) else {
        return;
    };
    let changed = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|mtime| mtime > doc.last_saved_time());
    let views: Vec<_> = doc.selections().keys().copied().collect();
    let Some(&first) = views.first().filter(|_| changed && !doc.is_modified()) else {
        return;
    };
    let trust_full = editor
        .workspace_trust
        .query(doc.workspace_root(), TrustQuery::Git)
        .is_trusted();
    let doc_id = doc.id();
    let doc = doc_mut!(editor, &doc_id);
    view_mut!(editor, first).sync_changes(doc);
    if let Err(err) = doc.reload(view_mut!(editor, first), &editor.diff_providers, trust_full) {
        log::warn!("couldn't reload {}: {err}", path.display());
        return;
    }
    editor
        .language_servers
        .file_event_handler
        .file_changed(path.to_owned());
    // As `:reload-all` does, so the other views don't refer to the old text.
    let scrolloff = editor.config().scrolloff;
    for view in views {
        let doc = doc_mut!(editor, &doc_id);
        let view = view_mut!(editor, view);
        view.sync_changes(doc);
        if view.doc == doc_id {
            view.ensure_cursor_in_view(doc, scrolloff);
        }
    }
}

/// Tells a client it can't join.
fn refuse(conn: &Conn, err: &anyhow::Error) {
    log::warn!("a client couldn't attach: {err:#}");
    conn.send(ServerMessage::Error(format!("{err:#}")));
    conn.send(ServerMessage::Exit(1));
}

impl Server {
    async fn next_event(&mut self) -> ServerEvent {
        loop {
            tokio::select! {
                Ok((conn, _)) = self.listener.accept() => self.handshake(conn),
                Some((stream, client)) = self.attached.1.recv() => {
                    match Conn::new(stream) {
                        Ok(conn) => return ServerEvent::Attached(conn, client),
                        Err(err) => log::warn!("lost a connection: {err}"),
                    }
                }
                Some((client, event)) = self.inputs.next() => {
                    return ServerEvent::Input(client, event);
                }
                Some((client, message)) = self.messages.1.recv() => {
                    return ServerEvent::Message(client, message);
                }
            }
        }
    }

    /// Attaches a new connection's terminal off the event loop.
    fn handshake(&self, conn: tokio::net::UnixStream) {
        let attached = self.attached.0.clone();
        let config = self.config.load_full();
        helix_event::client::spawn_blocking(move || {
            let Ok(conn) = conn
                .into_std()
                .and_then(|conn| conn.set_nonblocking(false).map(|()| conn))
            else {
                return;
            };
            let client = attach_terminal(&conn, &config).map(Box::new);
            let _ = attached.send((conn, client));
        });
    }

    fn add(&mut self, client: ClientId, mut conn: Conn, events: termina::EventStream) {
        if let Some(mut read) = conn.read.take() {
            let messages = self.messages.0.clone();
            helix_event::client::spawn(async move {
                loop {
                    let message = session::read_message_async(&mut read).await.ok();
                    let closed = message.is_none();
                    if messages.send((client, message)).is_err() || closed {
                        break;
                    }
                }
            });
        }
        self.conns.insert(client, conn);
        self.inputs.insert(client, events);
    }
}

impl Application {
    fn server(&mut self) -> &mut Server {
        self.server.as_mut().expect("this is a session server")
    }

    /// Returns false when the session is over.
    pub(super) async fn handle_server_event(&mut self, event: ServerEvent) -> bool {
        match event {
            ServerEvent::Attached(conn, Ok(client)) => self.attach(conn, *client).await,
            ServerEvent::Attached(conn, Err(err)) => refuse(&conn, &err),
            ServerEvent::Input(client, Ok(event)) => {
                self.handle_terminal_events(client, Ok(event)).await
            }
            ServerEvent::Input(client, Err(err)) => {
                log::warn!("lost the terminal of client {client}: {err}");
                return self.lose(client);
            }
            ServerEvent::Message(client, Some(ClientMessage::Resized)) => {
                self.resize_client(client).await
            }
            ServerEvent::Message(client, Some(ClientMessage::Continued)) => {
                self.resume_client(client).await
            }
            ServerEvent::Message(client, None) => return self.lose(client),
        }
        true
    }

    async fn attach(&mut self, conn: Conn, new: NewClient) {
        let NewClient {
            hello,
            stdin,
            backend,
            clipboard,
        } = new;
        let Some(backend) = backend else {
            self.open_remote(conn, &hello);
            self.render().await;
            return;
        };
        let server = self.server();
        let client = ClientId(server.next_client);
        server.next_client += 1;
        if let Err(err) = self.add_client(client, &hello, stdin, backend, clipboard) {
            self.detach(client);
            refuse(&conn, &err);
            return;
        }
        let events = self.event_stream();
        self.server().add(client, conn, events);
        self.focused = client;
        helix_event::dispatch(ClientAttached {
            editor: &mut self.editor,
            client,
        });
        self.render().await;
    }

    /// Sets up a client and opens what its command line asks for. Leaves it active.
    fn add_client(
        &mut self,
        client: ClientId,
        hello: &Hello,
        stdin: Option<Vec<u8>>,
        backend: TerminalBackend,
        clipboard: Option<Box<dyn Write + Send>>,
    ) -> anyhow::Result<()> {
        let theme_mode = backend.get_theme_mode();
        let terminal = Terminal::new(backend)?;
        let area = terminal.size();
        let mut compositor = Compositor::new(area);
        compositor.push(Self::editor_view(&self.config));
        // The client joins with the theme of the active one, the client last used, which may
        // come from `:theme` or Steel config. Not with a theme it's only previewing, nor with one
        // from config.toml, which can depend on each terminal's light or dark mode.
        let theme = self
            .editor
            .last_theme
            .as_ref()
            .unwrap_or(&self.editor.theme)
            .clone();
        let keep_theme = {
            let config = self.config.load();
            let configured = config
                .theme
                .as_ref()
                .map(|theme| theme.choose(self.theme_mode));
            let true_color = terminal.backend().supports_true_color()
                || config.editor.true_color
                || crate::true_color();
            configured != Some(theme.name()) && (true_color || theme.is_16_color())
        };
        let state = ClientState::new(
            theme,
            hello.cwd.clone(),
            environment(hello),
            hello.clipboard_provider.clone(),
        );
        self.editor.add_client(client, area, state);
        self.parked.insert(
            client,
            ClientUi {
                compositor,
                terminal,
                theme_mode,
                clipboard,
            },
        );
        self.activate(client);

        let args = client_args(hello)?;
        if let Some(delay) = args.auto_save {
            self.editor.client_auto_save.insert(client, delay);
        }
        for path in args.files.keys() {
            reload_if_changed(&mut self.editor, path);
        }
        let mut stdin = stdin.as_deref();
        Self::open_files(
            &mut self.editor,
            &mut self.compositor,
            args,
            stdin.as_mut().map(|stdin| stdin as &mut dyn Read),
        )?;
        if !keep_theme {
            Self::load_configured_theme(
                &mut self.editor,
                &self.config.load(),
                &mut self.terminal,
                self.theme_mode,
            );
        }
        self.terminal.claim()?;
        Ok(())
    }

    /// Suspends the clients that asked to be, and detaches those that have closed all their
    /// views. Returns false when that was all of them, which ends the session.
    pub(super) fn update_clients(&mut self) -> bool {
        if self.server.is_none() {
            return !self.editor.should_close();
        }
        for client in session::take_suspend_requests() {
            self.suspend_client(client);
        }
        handlers::auto_save::schedule_session_saves(&self.editor);
        let closed: Vec<_> = self.editor.tree.empty_clients().collect();
        if !closed.is_empty() {
            if !closed.into_iter().all(|client| self.detach(client)) {
                return false;
            }
            // They quit, so edits they leave behind go too: `:q` refuses to leave any, so
            // these are from `:q!`.
            self.close_orphans(true);
        }
        self.finish_waits();
        true
    }

    /// Opens a `--remote` command line's files in the focused client. Its `hx` exits once
    /// they're open, or with `--wait`, once none of them is shown.
    fn open_remote(&mut self, conn: Conn, hello: &Hello) {
        match self.open_remote_files(hello) {
            Ok((true, docs)) => {
                let client = self.focused;
                self.server().waits.push(Wait {
                    conn,
                    client,
                    docs,
                    code: 0,
                });
            }
            Ok((false, _)) => conn.send(ServerMessage::Exit(0)),
            Err(err) => refuse(&conn, &err),
        }
    }

    /// Returns whether the command line asks to wait, and the documents it opened.
    fn open_remote_files(&mut self, hello: &Hello) -> anyhow::Result<(bool, Vec<DocumentId>)> {
        // Relative paths are relative to where `hx --remote` ran.
        let cwd = helix_stdx::env::swap_current_working_dir(Some(hello.cwd.clone()));
        let args = Args::parse(hello.argv.iter().cloned());
        helix_stdx::env::swap_current_working_dir(cwd);
        let args = args?;
        let mut docs = Vec::new();
        for (path, positions) in args.files {
            reload_if_changed(&mut self.editor, &path);
            let doc_id = self.editor.open(&path, Action::Replace)?;
            let (view, doc) = current!(self.editor);
            let text = doc.text().slice(..);
            let selection = positions
                .into_iter()
                .map(|coords| Range::point(pos_at_coords(text, coords, true)))
                .collect();
            doc.set_selection(view.id, selection);
            align_view(doc, view, Align::Center);
            docs.push(doc_id);
        }
        Ok((args.wait, docs))
    }

    /// Tells the `--remote --wait` command lines whose files are no longer shown that they're
    /// done.
    fn finish_waits(&mut self) {
        if self.server().waits.is_empty() {
            return;
        }
        let shown: HashSet<_> = self.editor.tree.views().map(|(view, _)| view.doc).collect();
        self.server().waits.retain(|wait| {
            if wait.docs.iter().any(|doc| shown.contains(doc)) {
                return true;
            }
            wait.conn.send(ServerMessage::Exit(wait.code));
            false
        });
    }

    /// Detaches a client whose terminal or connection went away. Its edits stay, for the other
    /// clients to find.
    fn lose(&mut self, client: ClientId) -> bool {
        if !self.detach(client) {
            return false;
        }
        self.close_orphans(false);
        true
    }

    /// Closes the documents no view has open after a client left, so the next `hx` that opens
    /// one reads it fresh. Modified ones close only if `drop_edits`.
    fn close_orphans(&mut self, drop_edits: bool) {
        let orphans: Vec<_> = self
            .editor
            .documents()
            .filter(|doc| doc.selections().is_empty() && doc.name.is_none())
            .filter(|doc| drop_edits || !doc.is_modified())
            .map(|doc| doc.id())
            .collect();
        for doc in orphans {
            // Forced, so it can't fail.
            let _ = self.editor.close_document(doc, true);
        }
    }

    /// Lets a client go, because it quit or its terminal or connection is gone. Returns false
    /// if it's the last one, which ends the session: the server then tells it to exit once it
    /// has shut down.
    fn detach(&mut self, client: ClientId) -> bool {
        if !self.editor.clients().any(|c| c == client) {
            return true;
        }
        let Some(next) = self.editor.clients().find(|&c| c != client) else {
            return false;
        };
        let server = self.server();
        server.inputs.remove(&client);
        server.suspended.remove(&client);
        let conn = server.conns.remove(&client);

        self.activate(client);
        let code = self.editor.exit_code;
        let waits = self.server().waits.iter_mut();
        for wait in waits.filter(|wait| wait.client == client) {
            wait.code = code;
        }
        let _ = self.restore_term();
        if let Some(conn) = conn {
            conn.send_after(&mut self.terminal, vec![ServerMessage::Exit(code)]);
        }
        if self.focused == client {
            self.focused = next;
        }
        self.activate(self.focused);
        self.editor.remove_client(client);
        helix_event::dispatch(ClientDetached {
            editor: &mut self.editor,
            client,
        });
        self.parked.remove(&client);
        true
    }

    /// Gives a client's terminal back to its shell, and focuses another client if there's one
    /// still showing.
    fn suspend_client(&mut self, client: ClientId) {
        if self.server().suspended.contains(&client) || !self.activate(client) {
            return;
        }
        if let Err(err) = self.restore_term() {
            log::warn!("couldn't restore the terminal of client {client}: {err}");
        }
        let server = self.server.as_mut().expect("serving");
        // Stop reading the terminal, so the shell gets its input.
        server.inputs.remove(&client);
        server.suspended.insert(client);
        if let Some(conn) = server.conns.get(&client) {
            conn.send_after(&mut self.terminal, vec![ServerMessage::Suspend]);
        }
        if self.focused == client {
            let suspended = &self.server.as_ref().expect("serving").suspended;
            if let Some(next) = self.editor.clients().find(|c| !suspended.contains(c)) {
                self.focused = next;
            }
        }
        self.activate(self.focused);
    }

    async fn resume_client(&mut self, client: ClientId) {
        if !self.server().suspended.remove(&client) || !self.activate(client) {
            return;
        }
        if let Err(err) = self.reclaim_terminal() {
            log::warn!("couldn't take back the terminal of client {client}: {err}");
            self.detach(client);
            return;
        }
        let events = self.event_stream();
        self.server().inputs.insert(client, events);
        self.focused = client;
        self.render().await;
    }

    /// Handles a resize the client noticed, because the server gets no `SIGWINCH` for terminals
    /// that aren't its own.
    async fn resize_client(&mut self, client: ClientId) {
        let focused = self.focused;
        if self.server().suspended.contains(&client) || !self.activate(client) {
            return;
        }
        match self.terminal.backend().terminal().get_dimensions() {
            Ok(size) => {
                let event = termina::Event::WindowResized(size);
                self.handle_terminal_events(client, Ok(event)).await;
            }
            Err(err) => log::warn!("couldn't read the size of client {client}'s terminal: {err}"),
        }
        self.focused = focused;
        self.activate(focused);
    }

    /// Whether the active client takes no frames for now: it's suspended, or its terminal is
    /// behind. One that's behind is redrawn once it catches up.
    pub(super) fn skips_frames(&self) -> bool {
        self.terminal.backend().is_behind()
            || self
                .server
                .as_ref()
                .is_some_and(|server| server.suspended.contains(&self.editor.active_client()))
    }
}
