//! Coming back in the same terminal pane after the terminal restarts, with the Program Resume
//! Protocol (OSC 7577): <https://www.superlogical.com/rex/docs/build/program-resume.md>.
//!
//! A terminal that supports it sets a secret token in `OSC7577_TOKEN` for the first program in
//! each pane. `hx` reports the command that brings it back, `hx --resume <file>`, sealed with the
//! token. The file is a state file in `$XDG_STATE_HOME/helix/resume` with the editor's working
//! directory, session, config file, and auto-save delay, and its layout: the splits, the file,
//! selection, and scroll position of each view, and open buffers not shown in any view. After a
//! restart, the terminal runs that command in the pane.
//!
//! The report comes from the process in the pane: `hx` on its own, or a session's client while
//! the server isn't drawing to its terminal. The process that runs the editor writes the state
//! file: at startup, after a few seconds without input if the state changed, and when the
//! terminal goes away. Quitting clears the report and deletes the file. Losing the terminal
//! leaves both for the terminal to use when it brings the pane back.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    pin::Pin,
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use helix_core::{coords_at_pos, pos_at_coords, Position, Range, Selection, SmallVec};
use helix_view::{editor::Action, tree, view::ViewPosition, ClientId, Editor, ViewId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::time::{Instant, Sleep};

use crate::args::Args;

/// Maximum report length and `argv` count guaranteed by OSC 7577.
const MAX_LEN: usize = 2048;
const MAX_ARGS: usize = 64;
/// State files left behind this long ago, by an instance whose terminal never brought it back,
/// are deleted.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

static TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Takes the terminal's token out of the environment, so that nothing `hx` starts, a session's
/// server included, can report in its place. Call it before any thread starts.
pub fn take_token() {
    let token = std::env::var("OSC7577_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    std::env::remove_var("OSC7577_TOKEN");
    let _ = TOKEN.set(token);
}

/// The pane this process runs in, in a terminal that can bring it back there.
pub struct Pane {
    token: &'static str,
    /// The state file. Its path is part of the command that brings this instance back, so it
    /// stays the same for as long as the instance runs.
    path: PathBuf,
}

impl Pane {
    /// `None` unless the terminal supports resuming and this instance can be reported. `resume`
    /// is the state file passed to `--resume`, which this instance reuses.
    pub fn new(resume: Option<&Path>) -> Option<Self> {
        let token = TOKEN.get()?.as_deref()?;
        let dir = dir()?;
        let path = resume
            .map(Path::to_owned)
            .unwrap_or_else(|| dir.join(format!("{}-{}.json", unix_time(), std::process::id())));
        let pane = Self { token, path };
        pane.report_sequence()?;
        prune(&dir, &pane.path);
        Some(pane)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn report_sequence(&self) -> Option<Vec<u8>> {
        let argv = ["hx", "--resume", self.path.to_str()?];
        report(self.token, std::process::id(), unix_time(), &argv)
    }

    /// Tells the terminal how to bring this instance back. Call it only while nothing else
    /// draws to the terminal.
    pub fn report(&self) {
        if let Some(sequence) = self.report_sequence() {
            write_terminal(&sequence);
        }
    }

    /// Tells the terminal that the user ended this instance, and deletes its state file.
    pub fn clear(&self) {
        write_terminal(&clear(self.token, std::process::id(), unix_time()));
        let _ = fs::remove_file(&self.path);
    }
}

/// `$XDG_STATE_HOME/helix/resume`, created if it's missing. `None` if creating it fails or it
/// isn't private to this user, since its files control what the editor opens and are deleted on
/// quitting.
fn dir() -> Option<PathBuf> {
    let dir = helix_stdx::path::normalize(helix_loader::state_dir()?.join("resume"));
    #[cfg(unix)]
    let private = crate::create_private_dir(&dir);
    #[cfg(not(unix))]
    let private = Err(io::Error::from(io::ErrorKind::Unsupported));
    match private {
        Ok(()) => Some(dir),
        Err(err) => {
            log::warn!("can't resume: {err}");
            None
        }
    }
}

/// Returns true if `path` is a `.json` file directly inside the resume state directory.
/// Refusing arbitrary paths prevents `--resume` from overwriting or deleting unrelated files.
pub fn is_state_file(path: &Path) -> bool {
    dir().is_some_and(|dir| in_dir(path, &dir))
}

fn in_dir(path: &Path, dir: &Path) -> bool {
    path.parent() == Some(dir) && path.extension().is_some_and(|ext| ext == "json")
}

/// Deletes state files older than [`MAX_AGE`] (by mtime), except `keep`.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|time| time.elapsed().is_ok_and(|age| age > MAX_AGE));
        if old && in_dir(&entry.path(), dir) && entry.path() != keep {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

/// Writes to this process's terminal, the one a session's client hands its server.
#[cfg(unix)]
fn write_terminal(bytes: &[u8]) {
    let Some(terminal) = crate::terminal() else {
        return;
    };
    if let Err(err) = fs::File::from(terminal).write_all(bytes) {
        log::warn!("couldn't report to the terminal: {err}");
    }
}

/// No terminal takes reports here, since there's no state directory.
#[cfg(not(unix))]
fn write_terminal(_bytes: &[u8]) {}

/// The report that `argv` brings back the program `pid`, or `None` if it can't be reported.
fn report(token: &str, pid: u32, time: u64, argv: &[&str]) -> Option<Vec<u8>> {
    let printable = argv.iter().all(|arg| !arg.chars().any(char::is_control));
    if argv.len() > MAX_ARGS || !printable {
        return None;
    }
    let argv: Vec<_> = argv.iter().map(|arg| base64(arg.as_bytes())).collect();
    seal(
        token,
        &format!("pid={pid}:time={time}:argv={}", argv.join(",")),
    )
}

/// The report that removes the record of `pid`.
fn clear(token: &str, pid: u32, time: u64) -> Vec<u8> {
    seal(token, &format!("pid={pid}:time={time}:clear=1")).expect("a clear report is short")
}

/// The OSC 7577 sequence for `body`, with the MAC that shows it comes from the pane's program,
/// or `None` if it exceeds [`MAX_LEN`].
fn seal(token: &str, body: &str) -> Option<Vec<u8>> {
    let mac = Sha256::digest(format!("{body}:{token}"));
    let mac: String = mac.iter().map(|byte| format!("{byte:02x}")).collect();
    let sequence = format!("\x1b]7577;{body}:mac={mac}\x1b\\");
    (sequence.len() <= MAX_LEN).then(|| sequence.into_bytes())
}

/// Standard base64, with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk.iter().enumerate())
            .fold(0u32, |n, (i, &byte)| n | u32::from(byte) << (16 - 8 * i));
        for i in 0..4 {
            out.push(match i <= chunk.len() {
                true => ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char,
                false => '=',
            });
        }
    }
    out
}

/// Serialized editor state for `--resume`. Because the terminal restarts `hx` with no other
/// arguments and a fresh environment, this captures the original startup options as well as the
/// layout.
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub cwd: PathBuf,
    /// The session the instance was in, or `None` if it ran on its own.
    pub session: Option<String>,
    /// The config file from `--config`.
    pub config: Option<PathBuf>,
    /// The delay from `--auto-save`.
    pub auto_save: Option<u64>,
    /// `None` until the editor has written one, which a resumed instance takes as an empty
    /// command line.
    pub layout: Option<Layout>,
}

impl State {
    /// The state in `path`, or `None` if it's missing or unreadable, for `hx` to start as usual.
    pub fn load(path: &Path) -> Option<Self> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    pub fn write(&self, path: &Path) -> io::Result<()> {
        write(path, &serde_json::to_vec_pretty(self)?)
    }

    /// Applies this saved state to `args`.
    pub fn apply(self, args: &mut Args) {
        args.working_directory = Some(self.cwd).filter(|cwd| cwd.is_dir());
        args.config_file = self.config;
        args.auto_save = self.auto_save;
        match self.session {
            Some(key) => args.session = Some(key),
            None => args.no_session = true,
        }
        args.layout = self.layout;
    }
}

/// Atomically replaces `path` with mode 0600 so a crash never leaves a partial file.
fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut options = fs::File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    file.write_all(contents)?;
    drop(file);
    fs::rename(temp, path)
}

/// A client's views, how they're split, and any open buffers not shown in a view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub root: Node,
    /// The focused view, counting the views from the first.
    pub focus: usize,
    /// Open buffers not shown in any view, in open order.
    pub buffers: Vec<PathBuf>,
}

/// Splits are always equal, so a split has no sizes, only its children.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Node {
    Split {
        layout: tree::Layout,
        children: Vec<Node>,
    },
    View(SavedView),
}

/// Positions are lines and columns rather than offsets, so they still land near the same text
/// if the file changed a little, and clamp to its end if it got shorter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedView {
    /// `None` for a scratch buffer, which comes back empty.
    pub path: Option<PathBuf>,
    /// The anchor and head of each range.
    pub selection: Vec<(Position, Position)>,
    pub primary: usize,
    /// The first character the view shows, and the offsets of [`ViewPosition`] from it.
    pub top: Position,
    pub horizontal_offset: usize,
    pub vertical_offset: usize,
}

impl Layout {
    /// The active client's layout, or `None` if it has no views.
    pub fn capture(editor: &Editor) -> Option<Self> {
        let tree = &editor.tree;
        if tree.is_empty() {
            return None;
        }
        let shown: HashSet<_> = tree.views().map(|(view, _)| view.doc).collect();
        Some(Self {
            root: Node::capture(editor, tree.root()),
            focus: tree.traverse().position(|(id, _)| id == tree.focus)?,
            buffers: editor
                .documents()
                .filter(|doc| !shown.contains(&doc.id()))
                .filter_map(|doc| doc.path().map(Path::to_owned))
                .collect(),
        })
    }

    /// Rebuilds the layout in the active client, which has no views yet.
    pub fn restore(&self, editor: &mut Editor) {
        open(editor, self.root.first_view(), tree::Layout::Vertical);
        build(editor, &self.root, None);
        let mut saved = Vec::new();
        self.root.views(&mut saved);
        let views: Vec<_> = editor.tree.traverse().map(|(id, _)| id).collect();
        for (&view, saved) in views.iter().zip(saved) {
            saved.restore(editor, view);
        }
        for path in self.buffers.iter().filter(|path| path.exists()) {
            let _ = editor.open(path, Action::Load);
        }
        editor.focus(views.get(self.focus).copied().unwrap_or(views[0]));
    }
}

impl Node {
    fn capture(editor: &Editor, id: ViewId) -> Self {
        match editor.tree.container(id) {
            Some((layout, children)) => Self::Split {
                layout,
                children: (children.iter())
                    .map(|&child| Self::capture(editor, child))
                    .collect(),
            },
            None => Self::View(SavedView::capture(editor, id)),
        }
    }

    fn first_view(&self) -> Option<&SavedView> {
        match self {
            Self::Split { children, .. } => children.first()?.first_view(),
            Self::View(view) => Some(view),
        }
    }

    /// Adds the views in this node to `out`, in the order the tree has them.
    fn views<'a>(&'a self, out: &mut Vec<&'a SavedView>) {
        match self {
            Self::Split { children, .. } => children.iter().for_each(|child| child.views(out)),
            Self::View(view) => out.push(view),
        }
    }
}

/// Builds `node` in place of the focused view, which already shows the first view in it.
/// `parent` is the orientation of the enclosing split, or `None` at the root.
///
/// Creates one placeholder view per child first (showing each child's first view), then
/// recursively splits each placeholder.
fn build(editor: &mut Editor, node: &Node, parent: Option<tree::Layout>) {
    let Node::Split { layout, children } = node else {
        return;
    };
    let layout = *layout;
    let root = editor.tree.root();
    if parent.is_none()
        && editor
            .tree
            .container(root)
            .is_some_and(|(l, _)| l != layout)
    {
        // The root has one view, which is focused, so this turns the root.
        editor.tree.transpose();
    }
    let mut views = vec![editor.tree.focus];
    for child in children.iter().skip(1) {
        // Non-root containers are created on their first split. Splitting in the parent's
        // orientation would append to the parent instead, so split in the opposite orientation
        // and transpose.
        let nested = views.len() == 1 && parent == Some(layout);
        let way = match (nested, layout) {
            (false, layout) => layout,
            (true, tree::Layout::Vertical) => tree::Layout::Horizontal,
            (true, tree::Layout::Horizontal) => tree::Layout::Vertical,
        };
        open(editor, child.first_view(), way);
        if nested {
            editor.tree.transpose();
        }
        views.push(editor.tree.focus);
    }
    for (view, child) in views.into_iter().zip(children) {
        editor.tree.focus = view;
        build(editor, child, Some(layout));
    }
}

/// Splits the focused view along `layout` to open `view`'s file, or an empty scratch buffer if it
/// had none or it can't be opened.
fn open(editor: &mut Editor, view: Option<&SavedView>, layout: tree::Layout) {
    let action = match layout {
        tree::Layout::Horizontal => Action::HorizontalSplit,
        tree::Layout::Vertical => Action::VerticalSplit,
    };
    let path = view.and_then(|view| view.path.as_deref());
    if path.is_none_or(|path| editor.open(path, action).is_err()) {
        editor.new_file(action);
    }
}

impl SavedView {
    fn capture(editor: &Editor, id: ViewId) -> Self {
        let doc = &editor.documents[&editor.tree.get(id).doc];
        let text = doc.text().slice(..);
        let coords = |pos| coords_at_pos(text, pos);
        let selection = doc.selection(id);
        let offset = doc.view_offset(id);
        Self {
            path: doc.path().map(Path::to_owned),
            selection: (selection.iter())
                .map(|range| (coords(range.anchor), coords(range.head)))
                .collect(),
            primary: selection.primary_index(),
            top: coords(offset.anchor),
            horizontal_offset: offset.horizontal_offset,
            vertical_offset: offset.vertical_offset,
        }
    }

    fn restore(&self, editor: &mut Editor, id: ViewId) {
        let scrolloff = editor.config().scrolloff;
        let view = editor.tree.get(id);
        let doc = doc_mut!(editor, &view.doc);
        let text = doc.text().slice(..);
        let pos = |coords: Position| {
            let row = coords.row.min(text.len_lines() - 1);
            pos_at_coords(text, Position::new(row, coords.col), false)
        };
        let ranges: SmallVec<[Range; 1]> = (self.selection.iter())
            .map(|&(anchor, head)| Range::new(pos(anchor), pos(head)))
            .collect();
        // Clamped too, so that adding the view's size to them can't overflow. No view scrolls
        // past the end of the text.
        let offset = ViewPosition {
            anchor: pos(self.top),
            horizontal_offset: self.horizontal_offset.min(text.len_chars()),
            vertical_offset: self.vertical_offset.min(text.len_lines()),
        };
        if !ranges.is_empty() {
            let primary = self.primary.min(ranges.len() - 1);
            doc.set_selection(id, Selection::new(ranges, primary));
        }
        doc.set_view_offset(id, offset);
        // In case the file changed and the cursor isn't where the view was.
        view.ensure_cursor_in_view(doc, scrolloff);
    }
}

/// Keeps the state files of this process's clients up to date. A write waits until the editor
/// has had no input for [`idle`], and only happens if the state changed, so typing or moving
/// around never writes.
pub struct StateFiles {
    session: Option<String>,
    config: Option<PathBuf>,
    files: HashMap<ClientId, StateFile>,
    due: Option<Pin<Box<Sleep>>>,
}

struct StateFile {
    path: PathBuf,
    /// The client's `--auto-save` delay.
    auto_save: Option<u64>,
    /// What the file was last written with.
    written: Option<Vec<u8>>,
}

impl StateFiles {
    /// For an editor in `session`, or on its own if `None`, with `config` from `--config`.
    pub fn new(session: Option<String>, config: Option<PathBuf>) -> Self {
        Self {
            session,
            config,
            files: HashMap::new(),
            due: None,
        }
    }

    /// Keeps `client`'s state in the file at `path`. `auto_save` is the delay from `--auto-save`.
    pub fn add(&mut self, client: ClientId, path: PathBuf, auto_save: Option<u64>) {
        let file = StateFile {
            path,
            auto_save,
            written: None,
        };
        self.files.insert(client, file);
    }

    /// Stops writing `client`'s state file, which is the client's to delete or keep.
    pub fn remove(&mut self, client: ClientId) {
        self.files.remove(&client);
    }

    /// Resets the idle timer to write after [`idle`] without input. Called on input.
    pub fn postpone(&mut self) {
        if self.files.is_empty() {
            return;
        }
        let deadline = Instant::now() + idle();
        match &mut self.due {
            Some(due) => due.as_mut().reset(deadline),
            None => self.due = Some(Box::pin(tokio::time::sleep_until(deadline))),
        }
    }

    /// Schedules a write after [`idle`] if one isn't already pending, for changes that happen
    /// without input (such as an LSP response opening a file). Called after each frame.
    pub fn schedule(&mut self) {
        if self.due.is_none() && !self.files.is_empty() {
            self.due = Some(Box::pin(tokio::time::sleep(idle())));
        }
    }

    /// Resolves when the files are due to be written. Never, if nothing is scheduled.
    pub async fn due(&mut self) {
        match &mut self.due {
            Some(due) => due.await,
            None => std::future::pending().await,
        }
    }

    /// Cancels the pending write, since every client's file is about to be written.
    pub fn cancel(&mut self) {
        self.due = None;
    }

    /// Writes the active client's state file, if it has one and either its state changed since
    /// the last write or the file is gone. Another instance deletes a file left unwritten for
    /// [`MAX_AGE`].
    pub fn save(&mut self, editor: &Editor) {
        let Some(file) = self.files.get_mut(&editor.tree.active_client()) else {
            return;
        };
        let Some(layout) = Layout::capture(editor) else {
            return;
        };
        let state = State {
            cwd: helix_stdx::env::current_working_dir(),
            session: self.session.clone(),
            config: self.config.clone(),
            auto_save: file.auto_save,
            layout: Some(layout),
        };
        let Ok(contents) = serde_json::to_vec_pretty(&state) else {
            return;
        };
        if file.written.as_ref() == Some(&contents) && file.path.exists() {
            return;
        }
        match write(&file.path, &contents) {
            Ok(()) => file.written = Some(contents),
            Err(err) => log::warn!("couldn't write {}: {err}", file.path.display()),
        }
    }
}

/// How long the editor goes without input before state files are written. Tests shorten it with
/// `HELIX_RESUME_IDLE_MS`.
fn idle() -> Duration {
    static IDLE: OnceLock<Duration> = OnceLock::new();
    *IDLE.get_or_init(|| {
        let ms = std::env::var("HELIX_RESUME_IDLE_MS").ok();
        Duration::from_millis(ms.and_then(|ms| ms.parse().ok()).unwrap_or(3000))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hx"), "aHg=");
        assert_eq!(base64("é/".as_bytes()), "w6kv");
    }

    #[test]
    fn clear_matches_the_spec_example() {
        let sequence = clear("dummy-token-replace-me", 74877, 1795543200);
        assert_eq!(
            String::from_utf8(sequence).unwrap(),
            "\x1b]7577;pid=74877:time=1795543200:clear=1\
             :mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93\x1b\\"
        );
    }

    #[test]
    fn report_encodes_each_argument() {
        let sequence = report("token", 7, 100, &["hx", "--resume", "/s/a.json"]).unwrap();
        let sequence = String::from_utf8(sequence).unwrap();
        let body = "pid=7:time=100:argv=aHg=,LS1yZXN1bWU=,L3MvYS5qc29u";
        let mac = Sha256::digest(format!("{body}:token"));
        let mac: String = mac.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(sequence, format!("\x1b]7577;{body}:mac={mac}\x1b\\"));
        assert!(
            !sequence.contains("token"),
            "the token itself is never sent"
        );
    }

    #[test]
    fn rejects_invalid_reports() {
        let long = "a".repeat(MAX_LEN);
        assert!(report("t", 1, 1, &["hx", &long]).is_none());
        assert!(report("t", 1, 1, &["hx", "a\nb"]).is_none());
        assert!(report("t", 1, 1, &["hx"; MAX_ARGS + 1]).is_none());
        assert!(report("t", 1, 1, &["hx"; MAX_ARGS]).is_some());
    }

    #[test]
    fn resume_takes_only_state_files() {
        let dir = Path::new("/home/u/.local/state/helix/resume");
        assert!(in_dir(&dir.join("1-2.json"), dir));
        assert!(!in_dir(&dir.join("notes.txt"), dir));
        assert!(!in_dir(Path::new("/home/u/project/x.json"), dir));
        assert!(!in_dir(&dir.join("sub/1-2.json"), dir));
    }

    #[test]
    fn prune_deletes_only_old_state_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = SystemTime::now() - MAX_AGE - Duration::from_secs(60);
        for (name, time) in [
            ("old.json", old),
            ("kept.json", old),
            ("notes.txt", old),
            ("new.json", SystemTime::now()),
        ] {
            let file = fs::File::create(dir.path().join(name)).unwrap();
            file.set_modified(time).unwrap();
        }
        prune(dir.path(), &dir.path().join("kept.json"));
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["kept.json", "new.json", "notes.txt"]);
    }

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1-2.json");
        let view = |path: Option<&str>| {
            Node::View(SavedView {
                path: path.map(PathBuf::from),
                selection: vec![(Position::new(1, 2), Position::new(3, 4))],
                primary: 0,
                top: Position::new(1, 0),
                horizontal_offset: 0,
                vertical_offset: 0,
            })
        };
        let state = State {
            cwd: "/project".into(),
            session: Some("key".into()),
            config: None,
            auto_save: Some(500),
            layout: Some(Layout {
                root: Node::Split {
                    layout: tree::Layout::Vertical,
                    children: vec![view(Some("/project/a.txt")), view(None)],
                },
                focus: 1,
                buffers: vec!["/project/b.txt".into()],
            }),
        };
        state.write(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let loaded = State::load(&path).unwrap();
        assert_eq!(format!("{loaded:?}"), format!("{state:?}"));

        fs::write(&path, "not json").unwrap();
        assert!(State::load(&path).is_none());
        assert!(State::load(&dir.path().join("missing.json")).is_none());
    }
}
