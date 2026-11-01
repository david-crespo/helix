use std::{
    sync::{
        atomic::{self, AtomicBool},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::Ok;
use arc_swap::access::Access;

use helix_event::{register_hook, send_blocking};
use helix_view::{
    document::Mode,
    events::DocumentDidChange,
    handlers::{AutoSaveEvent, Handlers},
    ClientId, DocumentId, Editor,
};
use tokio::time::Instant;

use crate::{
    commands, compositor,
    events::{OnModeSwitch, TerminalFocusLost},
    job::{self, Jobs},
    session,
};

/// The documents edited in a session, each with the client that edited it, waiting for
/// [`schedule_session_saves`].
static EDITED: Mutex<Vec<(Option<ClientId>, DocumentId)>> = Mutex::new(Vec::new());

/// Times the saves of the documents edited in a session since the last call. A document saves
/// after the shortest delay of the clients that have it open and auto-save, whichever client
/// edited it. It takes the editor to tell, which hooks on edits don't have.
pub fn schedule_session_saves(editor: &Editor) {
    for (client, doc) in std::mem::take(&mut *EDITED.lock().unwrap()) {
        let delay = editor
            .document(doc)
            .and_then(|doc| editor.doc_auto_save_delay(doc));
        if let Some(save_after) = delay {
            let event = AutoSaveEvent::DocumentChanged { save_after };
            helix_event::client::sync_scope(client, || {
                send_blocking(&editor.handlers.auto_save, event)
            });
        }
    }
}

#[derive(Debug)]
pub(super) struct AutoSaveHandler {
    save_pending: Arc<AtomicBool>,
}

impl AutoSaveHandler {
    pub fn new() -> AutoSaveHandler {
        AutoSaveHandler {
            save_pending: Default::default(),
        }
    }
}

impl helix_event::AsyncHook for AutoSaveHandler {
    type Event = AutoSaveEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        existing_debounce: Option<tokio::time::Instant>,
    ) -> Option<Instant> {
        match event {
            Self::Event::DocumentChanged { save_after } => {
                Some(Instant::now() + Duration::from_millis(save_after))
            }
            Self::Event::LeftInsertMode => {
                if existing_debounce.is_some() {
                    // If the change happened more recently than the debounce, let the
                    // debounce run down before saving.
                    existing_debounce
                } else {
                    // Otherwise if there is a save pending, save immediately.
                    if self.save_pending.load(atomic::Ordering::Relaxed) {
                        self.finish_debounce();
                    }
                    None
                }
            }
        }
    }

    fn finish_debounce(&mut self) {
        let save_pending = self.save_pending.clone();
        job::dispatch_blocking(move |editor, _| {
            if editor.mode() == Mode::Insert {
                // Avoid saving while in insert mode since this mixes up
                // the modification indicator and prevents future saves.
                save_pending.store(true, atomic::Ordering::Relaxed);
            } else {
                request_auto_save(editor);
                save_pending.store(false, atomic::Ordering::Relaxed);
            }
        })
    }
}

fn request_auto_save(editor: &mut Editor) {
    // In a session, only the documents some client auto-saves.
    let docs = session::is_server().then(|| {
        let docs = editor.documents();
        let docs = docs.filter(|doc| editor.doc_auto_save_delay(doc).is_some());
        docs.map(|doc| doc.id()).collect()
    });
    save(editor, docs);
}

/// Saves the modified documents among `docs`, or all of them.
fn save(editor: &mut Editor, docs: Option<Vec<DocumentId>>) {
    let context = &mut compositor::Context {
        editor,
        scroll: Some(0),
        jobs: &mut Jobs::new(),
    };

    let options = commands::WriteAllOptions {
        force: false,
        write_scratch: false,
        auto_format: false,
        code_actions: false,
    };

    let saved = match docs {
        Some(docs) => commands::typed::write_docs_impl(context, options, |doc| docs.contains(&doc)),
        None => commands::typed::write_all_impl(context, options),
    };
    if let Err(e) = saved {
        context.editor.set_error(format!("{}", e));
    }
}

/// When a terminal loses focus, saves the modified documents, if auto-save says to. In a
/// session, the client that lost focus saves those it has open, if it auto-saves.
fn save_on_focus_lost(editor: &mut Editor) {
    let focus_lost = editor.config().auto_save.focus_lost;
    if !session::is_server() {
        if focus_lost {
            save(editor, None);
        }
        return;
    }
    let client = editor.active_client();
    if !focus_lost && !editor.client_auto_save.contains_key(&client) {
        return;
    }
    let docs = editor.documents();
    let docs = docs.filter(|doc| editor.clients_with(doc).any(|c| c == client));
    let docs = docs.map(|doc| doc.id()).collect();
    save(editor, Some(docs));
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.auto_save.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if session::is_server() {
            let edited = (helix_event::client::current(), event.doc.id());
            EDITED.lock().unwrap().push(edited);
            return Ok(());
        }
        let config = event.doc.config.load();
        if config.auto_save.after_delay.enable {
            send_blocking(
                &tx,
                AutoSaveEvent::DocumentChanged {
                    save_after: config.auto_save.after_delay.timeout,
                },
            );
        }
        Ok(())
    });

    let tx = handlers.auto_save.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            send_blocking(&tx, AutoSaveEvent::LeftInsertMode)
        }
        Ok(())
    });

    register_hook!(move |event: &mut TerminalFocusLost<'_, '_>| {
        save_on_focus_lost(event.cx.editor);
        Ok(())
    });
}
