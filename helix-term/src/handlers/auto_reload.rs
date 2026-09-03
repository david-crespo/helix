use std::fs;
use std::sync::atomic::{self, AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use helix_event::{register_hook, send_blocking};
use helix_view::document::Mode;
use helix_view::events::DocumentDidOpen;
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::{doc_mut, view, view_mut, Document, DocumentId, Editor, ViewId};
use tokio::time::Instant;

use crate::events::OnModeSwitch;
use crate::job;

#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    reload_pending: Arc<AtomicBool>,
}

impl AutoReloadHandler {
    pub fn new() -> AutoReloadHandler {
        AutoReloadHandler {
            reload_pending: Default::default(),
        }
    }
}

impl helix_event::AsyncHook for AutoReloadHandler {
    type Event = AutoReloadEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        existing_debounce: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            Self::Event::CheckForChanges { after } => {
                Some(Instant::now() + Duration::from_millis(after))
            }
            Self::Event::LeftInsertMode | Self::Event::EditorFocused => {
                if existing_debounce.is_some() {
                    // If the event happened more recently than the debounce, let the
                    // debounce run down before checking for changes.
                    existing_debounce
                } else {
                    // Otherwise if there is a reload pending, check immediately.
                    if self.reload_pending.load(Ordering::Relaxed) {
                        self.finish_debounce();
                    }
                    None
                }
            }
        }
    }

    fn finish_debounce(&mut self) {
        let reload_pending = self.reload_pending.clone();
        job::dispatch_blocking(move |editor, _| {
            if editor.mode() == Mode::Insert {
                // Avoid reloading while in insert mode since this mixes up
                // the modification indicator and prevents future saves.
                reload_pending.store(true, atomic::Ordering::Relaxed);
            } else {
                reload_externally_modified(editor);
                schedule_next_check(editor);
                reload_pending.store(false, atomic::Ordering::Relaxed);
            }
        });
    }
}

/// Reloads unmodified documents whose file changed on disk since helix last
/// saved them. Documents with unsaved edits are left alone: the next save will
/// fail with the usual "modified by an external process" error and the user
/// can `:reload` by hand. (Upstream #13963 prompted instead; #14544, the
/// accepted direction, reloads unmodified buffers silently, which is what we
/// do here.)
pub fn reload_externally_modified(editor: &mut Editor) {
    let scrolloff = editor.config().scrolloff;
    let focused_view = view!(editor).id;

    let stale: Vec<(DocumentId, Vec<ViewId>)> = editor
        .documents_mut()
        .filter(|doc| is_externally_modified(doc))
        .map(|doc| {
            let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();
            if view_ids.is_empty() {
                doc.ensure_view_init(focused_view);
                view_ids.push(focused_view);
            }
            (doc.id(), view_ids)
        })
        .collect();

    let mut reloaded = 0;
    for (doc_id, view_ids) in stale {
        let doc = doc_mut!(editor, &doc_id);
        let view = view_mut!(editor, view_ids[0]);
        view.sync_changes(doc);

        let trust_full = editor
            .workspace_trust
            .query(
                doc.workspace_root(),
                helix_loader::workspace_trust::TrustQuery::Git,
            )
            .is_trusted();
        if let Err(err) = doc.reload(view, &editor.diff_providers, trust_full) {
            editor.set_error(format!("Failed to reload document: {err}"));
            continue;
        }
        reloaded += 1;

        if let Some(path) = doc.path().map(ToOwned::to_owned) {
            editor
                .language_servers
                .file_event_handler
                .file_changed(path);
        }

        for view_id in view_ids {
            let view = view_mut!(editor, view_id);
            if view.doc == doc_id {
                view.sync_changes(doc);
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }

    if reloaded > 0 {
        editor.set_status(format!("Reloaded {reloaded} externally modified document(s)"));
    }
}

fn schedule_next_check(editor: &Editor) {
    let config = editor.config.load();
    if config.auto_reload.periodic.enable {
        let interval = config.auto_reload.periodic.interval;
        send_blocking(
            &editor.handlers.auto_reload,
            AutoReloadEvent::CheckForChanges { after: interval },
        );
    }
}

/// True for a document with no unsaved edits whose file on disk is newer than
/// helix's last save of it.
fn is_externally_modified(doc: &Document) -> bool {
    if doc.is_modified() {
        return false;
    }
    let Some(path) = doc.path() else {
        return false;
    };
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified > doc.last_saved_time(),
        Err(_) => false,
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let config = event.editor.config.load();
        if config.auto_reload.periodic.enable {
            let interval = config.auto_reload.periodic.interval;
            send_blocking(&tx, AutoReloadEvent::CheckForChanges { after: interval });
        }
        Ok(())
    });

    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            send_blocking(&tx, AutoReloadEvent::LeftInsertMode)
        }
        Ok(())
    });
}
