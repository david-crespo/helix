//! Utilities for declaring an async (usually debounced) hook

use tokio::time::Instant;

use crate::client::{self, ClientId, Receiver, Sender};

/// Async hooks provide a convenient framework for implementing (debounced)
/// async event handlers. Most synchronous event hooks will likely need to
/// debounce their events, coordinate multiple different hooks and potentially
/// track some state. `AsyncHooks` facilitate these use cases by running as
/// a background tokio task that waits for events (usually an enum) to be
/// sent through a channel.
///
/// Each event is handled as the client that sent it, and a debounce finishes as the client that
/// sent the last event, so what the hook dispatches reaches that client.
pub trait AsyncHook: Sync + Send + 'static + Sized {
    type Event: Sync + Send + 'static;
    /// Called immediately whenever an event is received, this function can
    /// consume the event immediately or debounce it. In case of debouncing,
    /// it can either define a new debounce timeout or continue the current one
    fn handle_event(&mut self, event: Self::Event, timeout: Option<Instant>) -> Option<Instant>;

    /// Called whenever the debounce timeline is reached
    fn finish_debounce(&mut self);

    fn spawn(self) -> Sender<Self::Event> {
        // the capacity doesn't matter too much here, unless the cpu is totally overwhelmed
        // the cap will never be reached since we always immediately drain the channel
        // so it should only be reached in case of total CPU overload.
        // However, a bounded channel is much more efficient so it's nice to use here
        let (tx, rx) = client::channel(128);
        // only spawn worker if we are inside runtime to avoid having to spawn a runtime for unrelated unit tests
        if tokio::runtime::Handle::try_current().is_ok() {
            client::spawn(run(self, rx));
        }
        tx
    }
}

async fn run<Hook: AsyncHook>(mut hook: Hook, mut rx: Receiver<Hook::Event>) {
    let mut deadline = None;
    let mut last_client: Option<ClientId> = None;
    loop {
        let event = match deadline {
            Some(deadline_) => {
                let res = tokio::time::timeout_at(deadline_, rx.recv()).await;
                match res {
                    Ok(event) => event,
                    Err(_) => {
                        client::sync_scope(last_client, || hook.finish_debounce());
                        deadline = None;
                        continue;
                    }
                }
            }
            None => rx.recv().await,
        };
        let Some((client, event)) = event else {
            break;
        };
        last_client = client;
        deadline = client::sync_scope(client, || hook.handle_event(event, deadline));
    }
}
