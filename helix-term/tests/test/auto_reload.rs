use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use helix_core::file_watcher::{
    CanonicalPathBuf, Event as FsEvent, EventType, FileSystemDidChange, WatchRootCrawled,
};
use helix_term::application::Application;
use helix_view::events::DocumentChangedOnDisk;
use helix_view::{doc, input::parse_macro, DocumentId};
use tempfile::NamedTempFile;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

use super::*;

#[cfg(windows)]
const FOCUS_GAINED: Event = Event::FocusGained;
#[cfg(not(windows))]
const FOCUS_GAINED: Event = Event::FocusIn;

/// Writes `text` to `path` as an external process would, `age` ago. Setting
/// the mtime avoids sleeping to get past the settle window.
fn write_externally(path: &Path, text: &str, age: Duration) {
    std::fs::write(path, text).unwrap();
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

/// Opens a file holding "a\nb\nc\n" that was last written well in the past,
/// so any later external write counts as a change. The file is outside the
/// workspace, so only the poll and focus checks see it, and the real watcher
/// is off: tests that need a watcher event dispatch one.
fn app() -> anyhow::Result<(NamedTempFile, Application)> {
    app_with(|_| {})
}

fn app_with(configure: impl FnOnce(&mut Config)) -> anyhow::Result<(NamedTempFile, Application)> {
    let file = NamedTempFile::new()?;
    write_externally(file.path(), "a\nb\nc\n", Duration::from_secs(10));
    let mut config = test_config();
    config.editor.file_watcher.enable = false;
    // Return from the event loop as soon as input is handled, before the poll
    // for a file left to settle (150ms) could fire.
    config.editor.idle_timeout = Duration::ZERO;
    configure(&mut config);
    let app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    Ok((file, app))
}

async fn send(app: &mut Application, events: impl IntoIterator<Item = Event>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for event in events {
        tx.send(Ok(event)).unwrap();
    }
    app.event_loop_until_idle(&mut UnboundedReceiverStream::new(rx))
        .await;
}

async fn keys(app: &mut Application, keys: &str) {
    let events = parse_macro(keys).unwrap().into_iter();
    send(app, events.map(|key| Event::Key(KeyEvent::from(key)))).await;
}

/// Reports a change to `path` as the file watcher would.
async fn watcher_event(app: &mut Application, path: &Path, ty: EventType) {
    let path = CanonicalPathBuf::assert_canonicalized(&path.canonicalize().unwrap());
    helix_event::dispatch(FileSystemDidChange {
        fs_events: vec![FsEvent { path, ty }].into(),
    });
    send(app, []).await;
}

/// Runs the event loop until `done`, for work that finishes in the background.
/// The event loop can wait for that work, so the timeout is around it.
async fn wait_for(app: &mut Application, done: impl Fn(&Application) -> bool) {
    let waiting = async {
        while !done(app) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            send(app, []).await;
        }
    };
    let waited = tokio::time::timeout(Duration::from_secs(10), waiting).await;
    assert!(waited.is_ok(), "timed out");
}

fn status(app: &Application) -> &str {
    app.editor.get_status().map_or("", |(msg, _)| msg)
}

#[tokio::test(flavor = "multi_thread")]
async fn focus_reloads_unmodified_buffer() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;

    assert_eq!(doc!(app.editor).text(), "a\nB\nc\n");
    assert!(!doc!(app.editor).is_modified());
    assert!(status(&app).ends_with("reloaded (external changes)"));
    Ok(())
}

/// The file is opened by a path through a symlink (the temp dir is one on
/// macOS); watcher event paths have symlinks resolved.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn watcher_event_reloads_through_symlink() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let real = dir.path().join("real.txt");
    let link = dir.path().join("link.txt");
    write_externally(&real, "a\nb\nc\n", Duration::from_secs(10));
    std::os::unix::fs::symlink(&real, &link)?;
    let mut config = test_config();
    config.editor.file_watcher.enable = false;
    config.editor.idle_timeout = Duration::ZERO;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&link, None)
        .build()?;

    write_externally(&real, "a\nB\nc\n", Duration::from_secs(5));
    watcher_event(&mut app, &real, EventType::Modified).await;
    assert_eq!(doc!(app.editor).text(), "a\nB\nc\n");
    Ok(())
}

/// A file deleted and recreated in separate watcher batches arrives as a
/// create event.
#[tokio::test(flavor = "multi_thread")]
async fn watcher_create_event_reloads() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    watcher_event(&mut app, file.path(), EventType::Create).await;
    assert_eq!(doc!(app.editor).text(), "a\nB\nc\n");
    Ok(())
}

/// A jj checkout empties the file, then writes the contents. Reading in
/// between would see an empty file, so a fresh write is left until it settles
/// and checked again then.
#[tokio::test(flavor = "multi_thread")]
async fn waits_for_fresh_write_to_settle() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    write_externally(file.path(), "", Duration::ZERO);
    send(&mut app, [FOCUS_GAINED]).await;
    assert_eq!(doc!(app.editor).text(), "a\nb\nc\n");

    write_externally(file.path(), "a\nB\nc\n", Duration::ZERO);
    tokio::time::sleep(Duration::from_millis(400)).await;
    send(&mut app, []).await;
    assert_eq!(doc!(app.editor).text(), "a\nB\nc\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn poll_reloads_unmodified_buffer() -> anyhow::Result<()> {
    let file = NamedTempFile::new()?;
    write_externally(file.path(), "a\nb\nc\n", Duration::from_secs(10));
    let mut config = test_config();
    config.editor.file_watcher.enable = false;
    config.editor.auto_reload.poll.interval = 20;
    // A poll more often than the idle timeout would keep the event loop busy.
    config.editor.idle_timeout = Duration::ZERO;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;

    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    wait_for(&mut app, |app| doc!(app.editor).text() == "a\nB\nc\n").await;
    Ok(())
}

/// A shorter interval set while a poll is scheduled applies without waiting
/// for that poll.
#[tokio::test(flavor = "multi_thread")]
async fn poll_interval_change_applies_now() -> anyhow::Result<()> {
    let file = NamedTempFile::new()?;
    write_externally(file.path(), "a\nb\nc\n", Duration::from_secs(10));
    let mut config = test_config();
    config.editor.file_watcher.enable = false;
    config.editor.auto_reload.poll.interval = 60_000;
    config.editor.idle_timeout = Duration::ZERO;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    keys(&mut app, ":set auto-reload.poll.interval 20<ret>").await;

    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    wait_for(&mut app, |app| doc!(app.editor).text() == "a\nB\nc\n").await;
    Ok(())
}

/// The real watcher, on a root outside the workspace. Its events come from
/// filesentry's thread, after its debounce.
#[tokio::test(flavor = "multi_thread")]
async fn real_watcher_reloads_unmodified_buffer() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("file.txt");
    write_externally(&path, "a\nb\nc\n", Duration::from_secs(10));
    let mut config = test_config();
    config.editor.idle_timeout = Duration::ZERO;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&path, None)
        .build()?;
    app.editor.file_watcher.add_root(dir.path());
    if !app.editor.file_watcher.is_watching(&path) {
        // No workspace around the tests to start the watcher in.
        return Ok(());
    }
    write_externally(&path, "a\nB\nc\n", Duration::from_secs(5));
    wait_for(&mut app, |app| doc!(app.editor).text() == "a\nB\nc\n").await;
    Ok(())
}

/// An open file in an ignored directory (the target dir, which this repo's
/// .gitignore lists), or outside the workspace if the target dir is elsewhere.
/// The workspace watch skips it, so the watcher watches it as an open file
/// rather than leaving it to the poll.
#[tokio::test(flavor = "multi_thread")]
async fn watcher_reloads_open_file_in_ignored_dir() -> anyhow::Result<()> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let path = dir.path().join("file.txt");
    write_externally(&path, "a\nb\nc\n", Duration::from_secs(10));
    let mut config = test_config();
    config.editor.idle_timeout = Duration::ZERO;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&path, None)
        .build()?;
    write_externally(&path, "a\nB\nc\n", Duration::from_secs(5));
    wait_for(&mut app, |app| doc!(app.editor).text() == "a\nB\nc\n").await;
    Ok(())
}

/// Turned off, an open file the workspace watch skips is polled again.
#[tokio::test(flavor = "multi_thread")]
async fn watch_open_files_off_leaves_file_to_poll() -> anyhow::Result<()> {
    let (file, mut app) = app_with(|config| config.editor.file_watcher.enable = true)?;
    assert!(app.editor.file_watcher.is_watching(file.path()));
    keys(&mut app, ":set file-watcher.watch-open-files false<ret>").await;
    assert!(!app.editor.file_watcher.is_watching(file.path()));
    Ok(())
}

/// Reloading in the middle of an insert keeps the two apart in the history:
/// undo takes back the reload, which came last, and leaves the insert.
#[tokio::test(flavor = "multi_thread")]
async fn reload_during_insert_is_its_own_undo_step() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    keys(&mut app, "iX").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    keys(&mut app, "y<ret><esc>").await;
    assert_eq!(doc!(app.editor).text(), "a\nb\nC\n");

    keys(&mut app, "u").await;
    assert_eq!(doc!(app.editor).text(), "Xa\nb\nc\n");
    keys(&mut app, "u").await;
    assert_eq!(doc!(app.editor).text(), "a\nb\nc\n");
    Ok(())
}

/// What each `DocumentChangedOnDisk` saw: the document, its text and whether
/// it was modified.
type ChangedOnDisk = Arc<Mutex<Vec<(DocumentId, String, bool)>>>;

/// Call after building the app, which registers the event.
fn record_changed_on_disk() -> ChangedOnDisk {
    let seen = ChangedOnDisk::default();
    let seen_ = seen.clone();
    helix_event::register_hook!(move |event: &mut DocumentChangedOnDisk<'_>| {
        let doc = doc!(event.editor, &event.doc);
        let entry = (event.doc, doc.text().to_string(), doc.is_modified());
        seen_.lock().unwrap().push(entry);
        Ok(())
    });
    seen
}

/// The paths of each `FileSystemDidChange`, which is what language servers are
/// told changed. Call after building the app, which registers the event.
fn record_fs_changes() -> Arc<Mutex<Vec<PathBuf>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_ = seen.clone();
    helix_event::register_hook!(move |event: &mut FileSystemDidChange| {
        let paths = event.fs_events.iter();
        let paths = paths.map(|event| event.path.as_std_path().to_owned());
        seen_.lock().unwrap().extend(paths);
        Ok(())
    });
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_on_disk_after_reloading_unmodified_buffer() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    let seen = record_changed_on_disk();
    let id = doc!(app.editor).id();
    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;

    assert_eq!(*seen.lock().unwrap(), [(id, "a\nB\nc\n".into(), false)]);
    Ok(())
}

/// Once per change: the buffer stays modified, so later checks find the same
/// change again.
#[tokio::test(flavor = "multi_thread")]
async fn changed_on_disk_for_modified_buffer() -> anyhow::Result<()> {
    let (file, mut app) = app_with(|config| config.editor.auto_reload.prompt_if_modified = false)?;
    let seen = record_changed_on_disk();
    let id = doc!(app.editor).id();
    keys(&mut app, "iX<esc>").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    send(&mut app, [FOCUS_GAINED]).await;

    assert_eq!(*seen.lock().unwrap(), [(id, "Xa\nb\nc\n".into(), true)]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_on_disk_waits_for_settle() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    let seen = record_changed_on_disk();
    write_externally(file.path(), "", Duration::ZERO);
    send(&mut app, [FOCUS_GAINED]).await;
    assert!(seen.lock().unwrap().is_empty());

    write_externally(file.path(), "a\nB\nc\n", Duration::ZERO);
    wait_for(&mut app, |_| !seen.lock().unwrap().is_empty()).await;
    assert_eq!(seen.lock().unwrap()[0].1, "a\nB\nc\n");
    Ok(())
}

/// By default a prompt asks about a modified buffer, and takes the next keys.
#[tokio::test(flavor = "multi_thread")]
async fn prompts_about_modified_buffer() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    keys(&mut app, "iX<esc>").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    keys(&mut app, "iY<esc>").await;

    assert_eq!(doc!(app.editor).text(), "Xa\nb\nc\n");
    assert!(status(&app).ends_with("external changes ignored"));
    Ok(())
}

/// With `notify-if-modified` off, a modified buffer is left to a hook: no
/// prompt takes the keys and no warning shows.
#[tokio::test(flavor = "multi_thread")]
async fn notify_if_modified_off_leaves_modified_buffer() -> anyhow::Result<()> {
    let (file, mut app) = app_with(|config| config.editor.auto_reload.notify_if_modified = false)?;
    let seen = record_changed_on_disk();
    keys(&mut app, "iX<esc>").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    assert_eq!(status(&app), "");
    keys(&mut app, "iY<esc>").await;

    assert_eq!(doc!(app.editor).text(), "XYa\nb\nc\n");
    assert_eq!(seen.lock().unwrap().len(), 1);
    Ok(())
}

/// The watcher can miss a change: one made before it first crawled the
/// directory, or while it couldn't watch. So the focus and poll checks cover
/// watched files too.
#[tokio::test(flavor = "multi_thread")]
async fn focus_reloads_watched_file() -> anyhow::Result<()> {
    let (file, mut app) = app_with(|config| config.editor.file_watcher.enable = true)?;
    assert!(app.editor.file_watcher.is_watching(file.path()));
    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;

    assert_eq!(doc!(app.editor).text(), "a\nB\nc\n");
    Ok(())
}

/// A watcher event for Helix's own save finds the mtime the save recorded.
#[tokio::test(flavor = "multi_thread")]
async fn own_save_is_not_a_change_on_disk() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    let seen = record_changed_on_disk();
    keys(&mut app, "iX<esc>:w<ret>").await;
    wait_for(&mut app, |app| !doc!(app.editor).is_modified()).await;
    let saved = status(&app).to_owned();
    // Past the settle window, so the event isn't just put off.
    tokio::time::sleep(Duration::from_millis(200)).await;
    watcher_event(&mut app, file.path(), EventType::Modified).await;

    assert_eq!(status(&app), saved);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(file.path())?, "Xa\nb\nc\n");
    Ok(())
}

/// The hook is for a document that has been reloaded, or couldn't be for its
/// unsaved changes. A reload that failed is neither. The failure is reported
/// once, not by every check that finds the file still changed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn no_changed_on_disk_after_failed_reload() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let (file, mut app) = app()?;
    let seen = record_changed_on_disk();
    let changes = record_fs_changes();
    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o000))?;
    send(&mut app, [FOCUS_GAINED]).await;

    assert!(
        status(&app).contains("auto-reload failed"),
        "{}",
        status(&app)
    );
    assert_eq!(doc!(app.editor).text(), "a\nb\nc\n");
    assert!(seen.lock().unwrap().is_empty());

    app.editor.clear_status();
    send(&mut app, [FOCUS_GAINED]).await;
    assert_eq!(status(&app), "");
    assert_eq!(changes.lock().unwrap().len(), 1);
    Ok(())
}

/// The prompt can open while the user is typing, so Enter alone, which may
/// have been meant for the buffer, doesn't discard the unsaved changes.
#[tokio::test(flavor = "multi_thread")]
async fn prompt_reloads_modified_buffer_only_on_y() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    keys(&mut app, "iX<esc>").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    keys(&mut app, "<ret>").await;
    assert_eq!(doc!(app.editor).text(), "Xa\nb\nc\n");
    assert!(status(&app).ends_with("external changes ignored"));

    write_externally(file.path(), "a\nb\nD\n", Duration::from_secs(4));
    send(&mut app, [FOCUS_GAINED]).await;
    keys(&mut app, "y<ret>").await;
    assert_eq!(doc!(app.editor).text(), "a\nb\nD\n");
    Ok(())
}

/// The document can be closed while the prompt about it is open.
#[tokio::test(flavor = "multi_thread")]
async fn prompt_for_closed_document_does_nothing() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    keys(&mut app, "iX<esc>").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    let id = doc!(app.editor).id();
    assert!(app.editor.close_document(id, true).is_ok());
    keys(&mut app, "y<ret>").await;
    assert!(app.editor.document(id).is_none());
    Ok(())
}

/// What is on disk when the watcher first crawls a root is its starting state,
/// so it never reports a write made before then. The end of the crawl sets off
/// a check instead.
#[tokio::test(flavor = "multi_thread")]
async fn end_of_crawl_reloads() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    write_externally(file.path(), "a\nB\nc\n", Duration::from_secs(5));
    helix_event::dispatch(WatchRootCrawled {});
    wait_for(&mut app, |app| doc!(app.editor).text() == "a\nB\nc\n").await;
    Ok(())
}

/// The watcher reports the end of a root's crawl, from its own thread.
#[tokio::test(flavor = "multi_thread")]
async fn watcher_reports_end_of_crawl() -> anyhow::Result<()> {
    let (file, mut app) = app_with(|config| config.editor.file_watcher.enable = true)?;
    if !app.editor.file_watcher.is_watching(file.path()) {
        // No watcher on this platform.
        return Ok(());
    }
    let crawled = Arc::new(Mutex::new(false));
    let crawled_ = crawled.clone();
    helix_event::register_hook!(move |_event: &mut WatchRootCrawled| {
        *crawled_.lock().unwrap() = true;
        Ok(())
    });
    let dir = tempfile::tempdir()?;
    app.editor.file_watcher.add_root(dir.path());
    wait_for(&mut app, |_| *crawled.lock().unwrap()).await;
    Ok(())
}

/// A check that finds a file just written leaves it for the check after it
/// settles, so language servers hear of the change once.
#[tokio::test(flavor = "multi_thread")]
async fn fresh_write_is_announced_once_settled() -> anyhow::Result<()> {
    let (file, mut app) = app()?;
    let changes = record_fs_changes();
    write_externally(file.path(), "a\nB\nc\n", Duration::ZERO);
    send(&mut app, [FOCUS_GAINED]).await;
    assert_eq!(*changes.lock().unwrap(), Vec::<PathBuf>::new());

    wait_for(&mut app, |app| doc!(app.editor).text() == "a\nB\nc\n").await;
    assert_eq!(*changes.lock().unwrap(), [file.path().canonicalize()?]);
    Ok(())
}

/// A conflict that was reported doesn't count once the unsaved changes are
/// undone: the buffer is unmodified, so the next check reloads it.
#[tokio::test(flavor = "multi_thread")]
async fn reloads_once_conflicting_changes_are_undone() -> anyhow::Result<()> {
    let (file, mut app) = app_with(|config| config.editor.auto_reload.prompt_if_modified = false)?;
    keys(&mut app, "iX<esc>").await;
    write_externally(file.path(), "a\nb\nC\n", Duration::from_secs(5));
    send(&mut app, [FOCUS_GAINED]).await;
    assert_eq!(doc!(app.editor).text(), "Xa\nb\nc\n");

    keys(&mut app, "u").await;
    send(&mut app, [FOCUS_GAINED]).await;
    assert_eq!(doc!(app.editor).text(), "a\nb\nC\n");
    Ok(())
}
