//! Runs real `hx` processes in pseudo-terminals and reads their screens through a VT parser.
//!
//! The tests only see what a user would: screens, files, exit codes and process state. How a
//! client finds its server is confined to [`Env::client`], so the suite can check any
//! implementation, including a reference build pointed to by `HX_BIN`.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub const TIMEOUT: Duration = Duration::from_secs(10);
/// How long a negative assertion ("this never shows up") watches before passing.
pub const SETTLE: Duration = Duration::from_millis(750);
const POLL: Duration = Duration::from_millis(20);
pub const ROWS: u16 = 24;
pub const COLS: u16 = 100;

fn hx_bin() -> PathBuf {
    std::env::var_os("HX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_hx").into())
}

/// One user on one machine: a private HOME, XDG dirs and runtime dir, so clients started
/// from the same `Env` find the same server and nothing reads the real user's config.
pub struct Env {
    root: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        Self::with_languages("")
    }

    /// `languages` is written to the user's `languages.toml`. `{root}` in it is replaced with
    /// the environment's root directory.
    pub fn with_languages(languages: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("hx-mc-")
            .tempdir()
            .expect("create temp dir");
        let env = Self { root };
        for dir in [
            "home/.config/helix",
            "cache",
            "data",
            "state",
            "run",
            "tmp",
            "project",
            // Makes the project a workspace, so clients in its subdirectories share a session.
            "project/.jj",
        ] {
            fs::create_dir_all(env.root().join(dir)).unwrap();
        }
        env.editor_config("");
        let languages = languages.replace("{root}", &env.root().display().to_string());
        env.write_abs("home/.config/helix/languages.toml", &languages);
        env
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The default working directory for clients.
    pub fn project(&self) -> PathBuf {
        self.root().join("project")
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.project().join(rel)
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn write_abs(&self, rel: &str, contents: &str) {
        fs::write(self.root().join(rel), contents).unwrap();
    }

    /// Writes a file in the user's config directory, such as Steel's `init.scm`.
    #[cfg(feature = "steel")]
    pub fn config_file(&self, name: &str, contents: &str) {
        self.write_abs(&format!("home/.config/helix/{name}"), contents);
    }

    /// Writes the user's `config.toml`, with `toml` added to its `[editor]` table.
    pub fn editor_config(&self, toml: &str) {
        // Keys are sent in legacy encoding. Without this, Helix would try to negotiate the
        // kitty keyboard protocol with a terminal that can't answer.
        self.write_abs(
            "home/.config/helix/config.toml",
            &format!(
                "[editor]\nkitty-keyboard-protocol = \"disabled\"\nsession = \"auto\"\n{toml}"
            ),
        );
    }

    /// Adds a theme to the user's config.
    pub fn theme(&self, name: &str, toml: &str) {
        fs::create_dir_all(self.root().join("home/.config/helix/themes")).unwrap();
        self.write_abs(&format!("home/.config/helix/themes/{name}.toml"), toml);
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap()
    }

    /// Writes an executable script under the root and returns its path.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.root().join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Starts `hx` with `args` in the project directory and waits for it to draw.
    pub fn client(&self, args: &[&str]) -> Client {
        self.client_in(&self.project(), args)
    }

    pub fn client_in(&self, cwd: &Path, args: &[&str]) -> Client {
        self.client_with(cwd, &[], args)
    }

    /// Starts a client with extra environment variables, as a shell with direnv would.
    pub fn client_with(&self, cwd: &Path, vars: &[(&str, &str)], args: &[&str]) -> Client {
        let mut cmd = self.command(hx_bin());
        cmd.args(args);
        cmd.cwd(cwd);
        for (key, value) in vars {
            cmd.env(key, value);
        }
        let client = self.spawn(cmd);
        client.wait_for("NOR");
        client
    }

    /// Runs a shell pipeline in a terminal, with `$HX` set to the binary under test. Used for
    /// things a plain client can't do, such as piping stdin.
    pub fn shell(&self, script: &str) -> Client {
        let mut cmd = self.command("/bin/sh".into());
        cmd.args(["-c", script]);
        cmd.env("HX", hx_bin());
        cmd.cwd(self.project());
        self.spawn(cmd)
    }

    /// An interactive bash with job control, for typing commands like `fg`.
    pub fn interactive_shell(&self) -> Client {
        let mut cmd = self.command("bash".into());
        cmd.args(["--norc", "--noprofile", "-i"]);
        cmd.env("HX", hx_bin());
        cmd.env("PS1", "$ ");
        cmd.cwd(self.project());
        let shell = self.spawn(cmd);
        shell.wait_for("$");
        shell
    }

    fn command(&self, program: PathBuf) -> CommandBuilder {
        let mut cmd = CommandBuilder::new(program);
        cmd.env_clear();
        let root = self.root();
        cmd.env("HOME", root.join("home"));
        cmd.env("XDG_CONFIG_HOME", root.join("home/.config"));
        cmd.env("XDG_CACHE_HOME", root.join("cache"));
        cmd.env("XDG_DATA_HOME", root.join("data"));
        cmd.env("XDG_STATE_HOME", root.join("state"));
        cmd.env("XDG_RUNTIME_DIR", root.join("run"));
        cmd.env("TMPDIR", root.join("tmp"));
        cmd.env("TERM", "xterm-256color");
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("SHELL", "/bin/sh");
        // The source tree's runtime, for grammars and queries.
        cmd.env(
            "HELIX_RUNTIME",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../runtime"),
        );
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd
    }

    fn spawn(&self, cmd: CommandBuilder) -> Client {
        let size = PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).expect("open pty");
        let child = pair.slave.spawn_command(cmd).expect("spawn hx");
        // Drop our copy of the slave so reads hit EOF once the program exits.
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(nz(ROWS), nz(COLS), 0)));
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let stopped = Arc::new((Mutex::new(false), Condvar::new()));
        let mut reader = pair.master.try_clone_reader().unwrap();
        {
            let parser = parser.clone();
            let output = output.clone();
            let writer = writer.clone();
            let stopped = stopped.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut tail = Vec::new();
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    output.lock().unwrap().extend_from_slice(&buf[..n]);
                    let mut parser = parser.lock().unwrap();
                    parser.process(&buf[..n]);
                    // Answer the queries Helix waits on at startup. The tail catches a query
                    // split across reads.
                    tail.extend_from_slice(&buf[..n]);
                    let reply = answer_queries(&tail, parser.screen());
                    if !reply.is_empty() {
                        let mut writer = writer.lock().unwrap();
                        let _ = writer.write_all(&reply);
                        let _ = writer.flush();
                    }
                    let keep = tail.len().saturating_sub(8);
                    tail.drain(..keep);
                    drop(parser);
                    let (stopped, resumed) = &*stopped;
                    drop(resumed.wait_while(stopped.lock().unwrap(), |stopped| *stopped));
                }
            });
        }

        Client {
            child,
            master: pair.master,
            writer,
            parser,
            output,
            stopped,
        }
    }
}

impl Drop for Env {
    /// Kills anything still running from this environment, such as a server, by finding
    /// processes with files open under its root.
    fn drop(&mut self) {
        let Ok(out) = Command::new("lsof")
            .args(["-t", "+D"])
            .arg(self.root())
            .output()
        else {
            return;
        };
        let me = std::process::id().to_string();
        for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if pid == me {
                continue;
            }
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    }
}

fn answer_queries(output: &[u8], screen: &vt100::Screen) -> Vec<u8> {
    let mut reply = Vec::new();
    let has = |query: &[u8]| output.windows(query.len()).any(|w| w == query);
    // Primary device attributes: claim a VT220 with nothing extra, so Helix stops waiting.
    if has(b"\x1b[c") || has(b"\x1b[0c") {
        reply.extend_from_slice(b"\x1b[?62;22c");
    }
    // Cursor position report.
    if has(b"\x1b[6n") {
        let (row, col) = screen.cursor_position();
        reply.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
    }
    reply
}

pub struct Client {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Everything the terminal received, including what the screen doesn't show.
    output: Arc<Mutex<Vec<u8>>>,
    /// Whether the terminal has stopped reading its program's output, and its resumption.
    stopped: Arc<(Mutex<bool>, Condvar)>,
}

impl Client {
    /// Types keys in Helix's notation: literal characters plus `<esc>`, `<ret>`, `<tab>`,
    /// `<bs>`, `<space>`, `<lt>`, `<gt>`, `<C-x>` and `<A-x>`.
    pub fn keys(&self, keys: &str) {
        let mut rest = keys;
        while !rest.is_empty() {
            let (bytes, after) = match rest.strip_prefix('<').and_then(|r| r.split_once('>')) {
                Some((name, after)) => (encode_key(name), after),
                None => {
                    let c = rest.chars().next().unwrap();
                    (c.to_string().into_bytes(), &rest[c.len_utf8()..])
                }
            };
            self.write(&bytes);
            // A lone ESC is only read as the Escape key if nothing follows it in the same read.
            if bytes == b"\x1b" {
                thread::sleep(Duration::from_millis(50));
            }
            rest = after;
        }
    }

    /// Runs a typed command, like `:w`, from normal mode.
    pub fn command(&self, command: &str) {
        self.keys(&format!(":{command}<ret>"));
    }

    fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }

    pub fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// One row of the screen, counting from 0 at the top.
    pub fn row(&self, row: u16) -> String {
        let parser = self.parser.lock().unwrap();
        let (_, cols) = parser.screen().size();
        let row = parser.screen().rows(0, cols.get()).nth(row as usize);
        row.unwrap_or_default()
    }

    /// The foreground color of the first cell showing `text`, if the screen shows it.
    pub fn color_of(&self, text: &str) -> Option<vt100::Color> {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        (0..rows.get()).find_map(|row| {
            let cells: Vec<_> = (0..cols.get())
                .map(|col| screen.cell(row, col).unwrap())
                .collect();
            let col = (0..cells.len()).find(|&col| {
                let mut chars = text.chars();
                cells[col..]
                    .iter()
                    .zip(chars.by_ref())
                    .all(|(cell, c)| cell.contents() == c.to_string())
                    && chars.next().is_none()
            })?;
            Some(cells[col].fgcolor())
        })
    }

    /// Whether the terminal has received `bytes`, such as an escape sequence.
    pub fn received(&self, bytes: &[u8]) -> bool {
        let output = self.output.lock().unwrap();
        output.windows(bytes.len()).any(|window| window == bytes)
    }

    /// The status line, the second row from the bottom.
    pub fn status_line(&self) -> String {
        let (rows, _) = self.parser.lock().unwrap().screen().size();
        self.row(rows.get() - 2)
    }

    pub fn wait_for(&self, text: &str) {
        self.wait_until(&format!("screen to show {text:?}"), |screen| {
            screen.contains(text)
        });
    }

    pub fn wait_until(&self, what: &str, done: impl Fn(&str) -> bool) {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if done(&screen) {
                return;
            }
            if start.elapsed() > TIMEOUT {
                panic!("timed out waiting for {what}. Screen:\n{screen}");
            }
            thread::sleep(POLL);
        }
    }

    /// Watches the screen for [`SETTLE`] and fails if `text` appears.
    pub fn never_shows(&self, text: &str) {
        let start = Instant::now();
        while start.elapsed() < SETTLE {
            let screen = self.screen();
            assert!(
                !screen.contains(text),
                "screen unexpectedly shows {text:?}:\n{screen}"
            );
            thread::sleep(POLL);
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.process_id().expect("client pid")
    }

    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for the client to exit and returns its exit code.
    pub fn wait_exit(&mut self) -> u32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            if start.elapsed() > TIMEOUT {
                panic!("timed out waiting for exit. Screen:\n{}", self.screen());
            }
            thread::sleep(POLL);
        }
    }

    /// Stops the terminal reading what its program writes, as a stalled SSH session would,
    /// until the returned guard is dropped. Writes to it block once the pty's buffer is full.
    pub fn stop_reading(&self) -> Stalled {
        *self.stopped.0.lock().unwrap() = true;
        Stalled(self.stopped.clone())
    }

    /// Sends SIGHUP to the client's process group, as closing its terminal would.
    pub fn hang_up(&self) {
        let group = format!("-{}", self.pid());
        Command::new("kill")
            .args(["-HUP", &group])
            .status()
            .unwrap();
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(nz(rows), nz(cols));
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }
}

pub struct Stalled(Arc<(Mutex<bool>, Condvar)>);

impl Drop for Stalled {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap() = false;
        self.0 .1.notify_all();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n).expect("terminal size must be nonzero")
}

fn encode_key(name: &str) -> Vec<u8> {
    match name {
        "esc" => b"\x1b".to_vec(),
        "ret" => b"\r".to_vec(),
        "tab" => b"\t".to_vec(),
        "bs" => b"\x7f".to_vec(),
        "space" => b" ".to_vec(),
        "lt" => b"<".to_vec(),
        "gt" => b">".to_vec(),
        _ => {
            if let Some(c) = name.strip_prefix("C-") {
                let c = c.chars().next().expect("key after C-");
                vec![(c.to_ascii_lowercase() as u8) & 0x1f]
            } else if let Some(c) = name.strip_prefix("A-") {
                let mut bytes = b"\x1b".to_vec();
                bytes.extend_from_slice(c.as_bytes());
                bytes
            } else {
                panic!("unknown key <{name}>")
            }
        }
    }
}
