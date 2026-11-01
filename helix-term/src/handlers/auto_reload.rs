use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use helix_core::file_watcher::{events_from_paths, Event, EventType, FileSystemDidChange};
use helix_event::{dispatch, register_hook, send_blocking};
use helix_loader::workspace_trust::TrustQuery;
use helix_view::editor::{Config, Severity};
use helix_view::events::{ConfigDidChange, DocumentChangedOnDisk};
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::{ClientId, DocumentId, Editor};
use tokio::time::Instant;

use crate::compositor::Compositor;
use crate::ui::{Prompt, PromptEvent};
use crate::{job, ui};

/// Whether the document's workspace is trusted for full git access.
fn doc_trust_full(editor: &Editor, doc_id: DocumentId) -> bool {
    let doc = doc!(editor, &doc_id);
    editor
        .workspace_trust
        .query(doc.workspace_root(), TrustQuery::Git)
        .is_trusted()
}

/// Handler for FileSystemDidChange events (from filesentry or polling)
struct ReloadHandler {
    enable: AtomicBool,
    prompt_if_modified: AtomicBool,
}

impl ReloadHandler {
    pub fn refresh_config(&self, config: &Config) {
        self.enable
            .store(config.auto_reload.enable, atomic::Ordering::Relaxed);
        self.prompt_if_modified.store(
            config.auto_reload.prompt_if_modified,
            atomic::Ordering::Relaxed,
        );
    }

    fn on_file_did_change(&self, event: &mut FileSystemDidChange) {
        if !self.enable.load(atomic::Ordering::Relaxed) {
            return;
        }
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
        let prompt_if_modified = self.prompt_if_modified.load(atomic::Ordering::Relaxed);
        // No client caused a change on disk, even when a client's focus set off the check: the
        // reload and the document-changed-on-disk hook run as no client, and the messages go to
        // the clients showing the document.
        let reload = move |editor: &mut Editor, compositor: &mut Compositor| {
            let vcs_reload = fs_events
                .iter()
                .any(|fs_event| editor.diff_providers.needs_reload(fs_event));

            // Event paths have symlinks resolved, document paths don't.
            let changed: HashSet<&Path> = fs_events
                .iter()
                .map(|fs_event| fs_event.path.as_std_path())
                .collect();
            let doc_ids: Vec<_> = editor
                .documents()
                .filter(|doc| {
                    doc.path()
                        .and_then(|path| path.canonicalize().ok())
                        .is_some_and(|path| changed.contains(path.as_path()))
                })
                .map(|doc| doc.id())
                .collect();
            for doc_id in doc_ids {
                handle_document_change(editor, compositor, doc_id, prompt_if_modified);
            }

            if vcs_reload {
                reload_vcs_diffs(editor);
            }
        };
        helix_event::client::sync_scope(None, || job::dispatch_blocking(reload));
    }
}

/// Handler for polling-based change detection for unwatched files.
/// Polls documents not covered by the file watcher (e.g., outside workspace or in ignored dirs).
/// Co-Authored-By: Anthony Rubick <68485672+AnthonyMichaelTDM@users.noreply.github.com>
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
            let config = editor.config();
            if !config.auto_reload.enable || !config.auto_reload.poll.enable {
                return;
            }

            // Check unwatched documents for external modifications
            let modified_paths = changed_unwatched_paths(editor);

            // Poll extra watched paths (e.g., VCS HEAD files outside workspace)
            let extra_path_changes = editor.file_watcher.poll_extra_paths();

            // Dispatch changes through the FileSystemDidChange hook
            let all_changed: Vec<PathBuf> = modified_paths
                .into_iter()
                .chain(extra_path_changes)
                .collect();
            if !all_changed.is_empty() {
                let events = events_from_paths(all_changed);
                dispatch(FileSystemDidChange { fs_events: events });
            }

            schedule_poll(&editor.handlers, &editor.config());
        });
    }
}

/// Open documents not covered by the file watcher (outside the workspace, or
/// ignored) whose on-disk mtime no longer matches their last save.
fn changed_unwatched_paths(editor: &Editor) -> Vec<PathBuf> {
    editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?;
            if editor.file_watcher.is_watching(path) {
                return None;
            }
            let mtime = path.metadata().ok()?.modified().ok()?;
            (mtime != doc.last_saved_time).then(|| path.to_path_buf())
        })
        .collect()
}

/// Re-check unwatched open files when the terminal regains focus -- they are only
/// polled otherwise, and regaining focus is when a user is most likely to have
/// just edited them elsewhere. Routes through the same `FileSystemDidChange` path
/// as the watcher and poll (so prompt de-duplication still applies).
pub(crate) fn on_focus_gained(editor: &Editor) {
    if !editor.config().auto_reload.enable {
        return;
    }
    let changed = changed_unwatched_paths(editor);
    if !changed.is_empty() {
        dispatch(FileSystemDidChange {
            fs_events: events_from_paths(changed),
        });
    }
}

const SETTLE: Duration = Duration::from_millis(150);

/// Handler for document changes detected by filesentry or polling
fn handle_document_change(
    editor: &mut Editor,
    compositor: &mut Compositor,
    doc_id: DocumentId,
    prompt_if_modified: bool,
) {
    let scrolloff = editor.config().scrolloff;
    let notify_if_modified = editor.config().auto_reload.notify_if_modified;
    let target_view_id = editor.get_synced_view_id(doc_id);
    let trust_full = doc_trust_full(editor, doc_id);

    let doc = doc_mut!(editor, &doc_id);
    let Some(path) = doc.path().map(ToOwned::to_owned) else {
        return;
    };

    let Ok(mtime) = path.metadata().and_then(|meta| meta.modified()) else {
        return;
    };

    if mtime == doc.last_saved_time {
        return;
    }

    // A writer that isn't atomic leaves the file briefly empty or partial: a jj
    // checkout truncates the file, then writes the contents. The watcher's
    // debounce doesn't cover the poll and focus checks, so leave a file written
    // within `SETTLE` for a check once it has settled.
    if SystemTime::now()
        .duration_since(mtime)
        .is_ok_and(|age| age < SETTLE)
    {
        log::info!(
            "auto-reload: {} was just written, waiting for it to settle",
            path.display()
        );
        helix_event::client::spawn(async move {
            tokio::time::sleep(SETTLE).await;
            job::dispatch(move |editor, compositor| {
                if editor.documents.contains_key(&doc_id) {
                    handle_document_change(editor, compositor, doc_id, prompt_if_modified);
                }
            })
            .await;
        });
        return;
    }

    if doc.is_modified() {
        // Surface a conflict (prompt / warning) once per distinct on-disk mtime;
        // the buffer is modified so we can't reload, and the poll / focus checks
        // would otherwise re-fire it on every tick.
        if doc.auto_reload_seen_mtime == Some(mtime) {
            return;
        }
        doc.auto_reload_seen_mtime = Some(mtime);
        if notify_if_modified {
            if prompt_if_modified {
                let path_str = doc
                    .relative_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "[scratch]".into());
                let client = prompt_client(editor, doc_id);
                if client == editor.active_client() {
                    prompt_reload_modified(compositor, doc_id, path_str);
                } else {
                    helix_event::client::sync_scope(Some(client), || {
                        job::dispatch_blocking(move |_, compositor| {
                            prompt_reload_modified(compositor, doc_id, path_str)
                        })
                    });
                }
            } else {
                let msg = format!(
                    "{} changed externally but has unsaved changes, use :reload to refresh",
                    doc.relative_path().unwrap().display()
                );
                notify(editor, doc_id, msg, Severity::Warning);
            }
        }
    } else {
        let view = view_mut!(editor, target_view_id);
        match doc.reload(view, &editor.diff_providers, trust_full) {
            Ok(_) => {
                view.ensure_cursor_in_view(doc, scrolloff);
                let msg = format!(
                    "{} reloaded (external changes)",
                    doc.relative_path().unwrap().display()
                );
                notify(editor, doc_id, msg, Severity::Info);
            }
            Err(err) => {
                let doc = doc!(editor, &doc_id);
                let msg = format!(
                    "{} auto-reload failed: {err}",
                    doc.relative_path().unwrap().display()
                );
                notify(editor, doc_id, msg, Severity::Error);
            }
        }
    }

    dispatch(DocumentChangedOnDisk {
        editor,
        doc: doc_id,
    });
}

/// Shows a message about `doc_id` in the clients showing it, or if none does, in the active
/// client, as a standalone editor shows it whichever buffer is open.
fn notify(editor: &mut Editor, doc_id: DocumentId, msg: String, severity: Severity) {
    let mut clients = editor.clients_showing(doc_id);
    if clients.is_empty() {
        clients.push(editor.active_client());
    }
    for client in clients {
        let msg = msg.clone();
        helix_event::client::sync_scope(Some(client), || {
            editor.set_status_with_severity(msg, severity)
        });
    }
}

/// The one client to ask whether to reload `doc_id`: a client whose focused view shows it, else
/// one showing it, else the active client. The active client comes first, which, as reloads run
/// as no client, is the one that last had input.
fn prompt_client(editor: &Editor, doc_id: DocumentId) -> ClientId {
    let showing = editor.clients_showing(doc_id);
    showing
        .iter()
        .find(|&&client| editor.client_focuses(client, doc_id))
        .or(showing.first())
        .copied()
        .unwrap_or_else(|| editor.active_client())
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

/// Shows a prompt asking the user whether to reload a modified document.
/// Co-Authored-By: Anthony Rubick <68485672+AnthonyMichaelTDM@users.noreply.github.com>
fn prompt_reload_modified(compositor: &mut Compositor, doc_id: DocumentId, path_str: String) {
    let prompt = Prompt::new(
        Cow::Owned(format!(
            "{path_str} changed externally (unsaved changes exist). Press Enter to reload, Esc to ignore: "
        )),
        None,
        ui::completers::none,
        move |cx, _input, event| {
            match event {
                PromptEvent::Validate => {
                    let scrolloff = cx.editor.config().scrolloff;
                    let target_view_id = cx.editor.get_synced_view_id(doc_id);
                    let trust_full = doc_trust_full(cx.editor, doc_id);
                    let doc = doc_mut!(cx.editor, &doc_id);
                    let view = view_mut!(cx.editor, target_view_id);
                    match doc.reload(view, &cx.editor.diff_providers, trust_full) {
                        Ok(_) => {
                            view.ensure_cursor_in_view(doc, scrolloff);
                            cx.editor.set_status(format!("{path_str} reloaded"));
                        }
                        Err(err) => {
                            cx.editor
                                .set_error(format!("{path_str} reload failed: {err}"));
                        }
                    }
                }
                PromptEvent::Abort => {
                    cx.editor
                        .set_status(format!("{path_str} external changes ignored"));
                }
                PromptEvent::Update => {}
            }
        },
    );
    compositor.push(Box::new(prompt));
}

pub(super) fn register_hooks(handlers: &Handlers, config: &Config) {
    // Register handler for FileSystemDidChange events (from filesentry)
    let handler = Arc::new(ReloadHandler {
        enable: config.auto_reload.enable.into(),
        prompt_if_modified: config.auto_reload.prompt_if_modified.into(),
    });
    let handler_ = handler.clone();
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        handler_.refresh_config(event.new);
        // A shorter interval applies now rather than after the poll already
        // scheduled, and turning polling back on restarts it.
        schedule_poll(&event.editor.handlers, event.new);
        Ok(())
    });
    register_hook!(move |event: &mut FileSystemDidChange| {
        handler.on_file_did_change(event);
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
