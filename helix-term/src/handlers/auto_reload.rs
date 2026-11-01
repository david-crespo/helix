use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use helix_core::file_watcher::{
    events_from_paths, Event, EventType, FileSystemDidChange, WatchRootCrawled,
};
use helix_event::{dispatch, register_hook, send_blocking};
use helix_loader::workspace_trust::TrustQuery;
use helix_view::editor::Config;
use helix_view::events::{ConfigDidChange, DocumentChangedOnDisk};
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::{Document, DocumentId, Editor};
use tokio::time::Instant;

use crate::compositor::Compositor;
use crate::ui::{Prompt, PromptEvent};
use crate::{job, ui};

/// Reloads the open documents that changed on disk, for a batch of events from
/// the file watcher or from a poll or focus check.
fn on_file_did_change(event: &mut FileSystemDidChange) {
    // A file deleted and recreated in separate settle windows (some VCS
    // operations) is reported as created, not modified.
    let fs_events: Vec<Event> = event
        .fs_events
        .iter()
        .filter(|event| matches!(event.ty, EventType::Modified | EventType::Create))
        .cloned()
        .collect();
    if fs_events.is_empty() {
        return;
    }
    job::dispatch_blocking(move |editor, compositor| {
        if !editor.config().auto_reload.enable {
            return;
        }
        // Every document is checked, not only the ones the events are for: the
        // check is one stat each, and event paths have symlinks resolved where
        // document paths don't.
        let doc_ids: Vec<_> = editor.documents().map(|doc| doc.id()).collect();
        for doc_id in doc_ids {
            handle_document_change(editor, compositor, doc_id);
        }

        if fs_events
            .iter()
            .any(|fs_event| editor.diff_providers.needs_reload(fs_event))
        {
            reload_vcs_diffs(editor);
        }
    });
}

/// Runs the periodic check, and the check of a file left to settle.
#[derive(Debug)]
pub(super) struct PollHandler;

impl PollHandler {
    pub fn new() -> Self {
        PollHandler
    }
}

impl helix_event::AsyncHook for PollHandler {
    type Event = AutoReloadEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        existing_debounce: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            // The sooner of this and a poll already scheduled, so a config change
            // never puts one off.
            AutoReloadEvent::PollAfter { interval } => {
                let next = Instant::now() + Duration::from_millis(interval);
                Some(existing_debounce.map_or(next, |existing| existing.min(next)))
            }
        }
    }

    fn finish_debounce(&mut self) {
        job::dispatch_blocking(move |editor, _compositor| {
            check_documents(editor);
            schedule_poll(&editor.handlers, &editor.config());
        });
    }
}

/// The mtime of the document's file if it has a change the document hasn't
/// taken: it differs from the last save or reload, and isn't a conflict with
/// unsaved changes or a failed reload that was already reported. A conflicted
/// buffer undone back to unmodified has a change again.
fn changed_mtime(doc: &Document) -> Option<SystemTime> {
    let mtime = doc.path()?.metadata().ok()?.modified().ok()?;
    let reported = doc.auto_reload_reported == Some((mtime, doc.is_modified()));
    (mtime != doc.last_saved_time && !reported).then_some(mtime)
}

/// Checks every open document against its file, on a poll and when the terminal
/// regains focus. The watcher can miss a change (one made before it first
/// crawled the directory, or while a watch failed), so this covers watched
/// files too. Goes through `FileSystemDidChange` as watcher events do, so
/// language servers are told as well.
pub(crate) fn check_documents(editor: &mut Editor) {
    if !editor.config().auto_reload.enable {
        return;
    }
    // A file still settling is left out, and so announced once, by the poll
    // after it has settled.
    let (settling, settled): (Vec<_>, Vec<_>) = editor
        .documents()
        .filter_map(|doc| Some((changed_mtime(doc)?, doc.path()?.to_owned())))
        .partition(|(mtime, _)| is_settling(*mtime));
    if !settling.is_empty() {
        poll_once_settled(editor);
    }
    let mut changed: Vec<PathBuf> = settled.into_iter().map(|(_, path)| path).collect();
    // VCS HEAD files outside the workspace
    changed.extend(editor.file_watcher.poll_extra_paths());
    if !changed.is_empty() {
        dispatch(FileSystemDidChange {
            fs_events: events_from_paths(changed),
        });
    }
}

const SETTLE: Duration = Duration::from_millis(150);

/// Whether the file was written within `SETTLE`. A writer that isn't atomic
/// leaves the file briefly empty or partial: a jj checkout truncates the file,
/// then writes the contents. So a file just written is left for a poll once it
/// has settled.
fn is_settling(mtime: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(mtime)
        .is_ok_and(|age| age < SETTLE)
}

fn poll_once_settled(editor: &Editor) {
    let interval = SETTLE.as_millis() as u64;
    send_blocking(
        &editor.handlers.auto_reload,
        AutoReloadEvent::PollAfter { interval },
    );
}

/// Reloads the document if its file changed, or reports the change if the
/// document has unsaved changes.
fn handle_document_change(editor: &mut Editor, compositor: &mut Compositor, doc_id: DocumentId) {
    let doc = doc!(editor, &doc_id);
    let Some(mtime) = changed_mtime(doc) else {
        return;
    };

    // The poll and focus checks leave out a file that is settling. Watcher
    // events get here with one: every document is checked for any event, and
    // the watcher's debounce is its own.
    if is_settling(mtime) {
        log::info!(
            "auto-reload: {} was just written, waiting for it to settle",
            doc.display_name()
        );
        poll_once_settled(editor);
        return;
    }

    let config = editor.config();
    let doc = doc_mut!(editor, &doc_id);
    // If this ends in a conflict or a failed reload, that is reported once:
    // `changed_mtime` skips the document from here on.
    doc.auto_reload_reported = Some((mtime, doc.is_modified()));
    if doc.is_modified() {
        if config.auto_reload.notify_if_modified {
            if config.auto_reload.prompt_if_modified {
                let path_str = doc
                    .relative_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "[scratch]".into());
                prompt_reload_modified(compositor, doc_id, path_str);
            } else {
                let msg = format!(
                    "{} changed externally but has unsaved changes, use :reload to refresh",
                    doc.relative_path().unwrap().display()
                );
                editor.set_warning(msg);
            }
        }
    } else {
        let reloaded = editor.reload_document(doc_id);
        let path = doc!(editor, &doc_id).relative_path().unwrap();
        if let Err(err) = reloaded {
            editor.set_error(format!("{} auto-reload failed: {err}", path.display()));
            return;
        }
        editor.set_status(format!("{} reloaded (external changes)", path.display()));
    }

    dispatch(DocumentChangedOnDisk {
        editor,
        doc: doc_id,
    });
}

/// Reload VCS diffs for all documents
fn reload_vcs_diffs(editor: &mut Editor) {
    for doc in editor.documents.values_mut() {
        let Some(path) = doc.path() else {
            continue;
        };
        let trust_full = editor
            .workspace_trust
            .query(doc.workspace_root(), TrustQuery::Git)
            .is_trusted();
        match editor.diff_providers.get_diff_base(path, trust_full) {
            Some(diff_base) => doc.set_diff_base(diff_base),
            None => doc.diff_handle = None,
        }
    }
}

/// Asks whether to reload a document with unsaved changes. Only `y` reloads:
/// the prompt can open while the user is typing, and their next Enter shouldn't
/// discard the changes.
fn prompt_reload_modified(compositor: &mut Compositor, doc_id: DocumentId, path_str: String) {
    let prompt = Prompt::new(
        Cow::Owned(format!(
            "{path_str} changed on disk and has unsaved changes. Reload and discard them? (y/n) "
        )),
        None,
        ui::completers::none,
        move |cx, input, event| {
            let reload = match event {
                PromptEvent::Update => return,
                PromptEvent::Validate => input.trim().eq_ignore_ascii_case("y"),
                PromptEvent::Abort => false,
            };
            // The document may have been closed while the prompt was open.
            if cx.editor.document(doc_id).is_none() {
                return;
            }
            if !reload {
                cx.editor
                    .set_status(format!("{path_str} external changes ignored"));
                return;
            }
            match cx.editor.reload_document(doc_id) {
                Ok(()) => cx.editor.set_status(format!("{path_str} reloaded")),
                Err(err) => cx
                    .editor
                    .set_error(format!("{path_str} reload failed: {err}")),
            }
        },
    );
    compositor.push(Box::new(prompt));
}

pub(super) fn register_hooks(handlers: &Handlers, config: &Config) {
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        // A shorter interval applies now rather than after the poll already
        // scheduled, and turning polling back on restarts it.
        schedule_poll(&event.editor.handlers, event.new);
        Ok(())
    });
    register_hook!(move |event: &mut FileSystemDidChange| {
        on_file_did_change(event);
        Ok(())
    });
    // A write between Helix reading a file and the watcher's first crawl
    // reaching it is never reported, so check once the crawl is done.
    let poll = handlers.auto_reload.clone();
    register_hook!(move |_event: &mut WatchRootCrawled| {
        send_blocking(&poll, AutoReloadEvent::PollAfter { interval: 0 });
        Ok(())
    });

    schedule_poll(handlers, config);
}

fn schedule_poll(handlers: &Handlers, config: &Config) {
    if config.auto_reload.enable && config.auto_reload.poll.enable {
        send_blocking(
            &handlers.auto_reload,
            AutoReloadEvent::PollAfter {
                interval: config.auto_reload.poll.interval,
            },
        );
    }
}
