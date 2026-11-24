//! Stops language servers that have had no documents for `editor.lsp.unused-server-timeout`.
//!
//! A server keeps running for the timeout after its last document closes, so reopening a file
//! soon after doesn't wait for it to start again. Without this, it would run until the editor
//! exits, which in a session is when the last client leaves.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use helix_event::send_blocking;
use helix_lsp::LanguageServerId;
use helix_view::handlers::lsp::UnusedServersEvent;
use helix_view::Editor;
use tokio::time::Instant;

use crate::job;

/// How long a stopped server has to answer the shutdown request and exit before its process
/// is killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The servers that may have no documents, with when each lost its last one.
type Unused = HashMap<LanguageServerId, Instant>;

#[derive(Debug, Default)]
pub(super) struct Handler {
    unused: Arc<Mutex<Unused>>,
}

impl helix_event::AsyncHook for Handler {
    type Event = UnusedServersEvent;

    fn handle_event(&mut self, event: Self::Event, existing: Option<Instant>) -> Option<Instant> {
        let at = match event {
            UnusedServersEvent::DocumentClosed { servers, timeout } => {
                let now = Instant::now();
                let mut unused = self.unused.lock().unwrap();
                // A server that had a document until now starts its timeout over, even if it
                // lost an earlier one.
                for server in servers {
                    unused.insert(server, now);
                }
                now + timeout
            }
            UnusedServersEvent::CheckAt(at) => at,
        };
        Some(existing.map_or(at, |existing| existing.min(at)))
    }

    fn finish_debounce(&mut self) {
        let unused = self.unused.clone();
        // Stopping a server is no client's doing.
        helix_event::client::sync_scope(None, || {
            job::dispatch_blocking(move |editor, _| stop_unused(editor, &unused))
        });
    }
}

/// Stops the servers that have had no documents for the timeout, forgets the ones that have one
/// again, and schedules a check for the rest. A timeout of zero forgets them all.
fn stop_unused(editor: &mut Editor, unused: &Mutex<Unused>) {
    let timeout = editor.config().lsp.unused_server_timeout;
    let now = Instant::now();
    let mut next = None::<Instant>;
    let mut due = Vec::new();
    unused.lock().unwrap().retain(|&server, &mut since| {
        if timeout.is_zero()
            || editor.language_server_by_id(server).is_none()
            || editor.language_server_has_documents(server)
        {
            return false;
        }
        let at = since + timeout;
        if at <= now {
            due.push(server);
            return false;
        }
        next = Some(next.map_or(at, |next| next.min(at)));
        true
    });
    for server in due {
        stop(editor, server);
    }
    if let Some(at) = next {
        send_blocking(
            &editor.handlers.unused_servers,
            UnusedServersEvent::CheckAt(at),
        );
    }
}

/// Removes the server from the registry, so the next document that needs it starts it again,
/// then shuts it down.
fn stop(editor: &mut Editor, server: LanguageServerId) {
    let Some(client) = editor.language_servers.get_by_id(server).cloned() else {
        return;
    };
    log::info!(
        "stopping language server {} with no documents",
        client.name()
    );
    editor.language_servers.remove_by_id(server);
    // The client kills the process when dropped, so it's held until the server has exited.
    helix_event::client::spawn(async move {
        let shutdown = async {
            if let Err(err) = client.shutdown_and_exit().await {
                log::warn!(
                    "language server {} failed to shut down: {err}",
                    client.name()
                );
                return;
            }
            client.wait_for_process_exit().await;
        };
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown).await;
    });
}
