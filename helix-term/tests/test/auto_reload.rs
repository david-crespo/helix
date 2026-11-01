use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use helix_core::file_watcher::{
    CanonicalPathBuf, Event as FsEvent, EventType, FileSystemDidChange,
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
    // Return from the event loop as soon as input is handled, before a
    // settle recheck (150ms) could fire.
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

/// Writes `text` to `path` until the buffer shows it. A write before
/// filesentry has crawled the file's directory is taken as the starting state,
/// not reported, so write again after each debounce.
async fn write_until_reloaded(app: &mut Application, path: &Path, text: &str) {
    let reloading = async {
        while doc!(app.editor).text() != text {
            write_externally(path, text, Duration::from_secs(5));
            tokio::time::sleep(Duration::from_millis(500)).await;
            send(app, []).await;
        }
    };
    let reloaded = tokio::time::timeout(Duration::from_secs(10), reloading).await;
    assert!(reloaded.is_ok(), "timed out");
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
    write_until_reloaded(&mut app, &path, "a\nB\nc\n").await;
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
    write_until_reloaded(&mut app, &path, "a\nB\nc\n").await;
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
    keys(&mut app, "<ret><esc>").await;
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
