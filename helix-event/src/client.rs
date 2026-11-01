//! Which client work is for, when several terminals share one editor.
//!
//! The main thread switches between clients as it handles their input. Work it hands off, to a
//! task, a job or an async hook, carries the client along, so that what comes back (a status
//! message, a popup) reaches the client that asked for it. Work no client caused, such as a
//! language server's notifications, runs as no client.

use std::cell::Cell;
use std::future::Future;
use std::time::Duration;

use futures_executor::block_on;
use helix_stdx::env::Snapshot;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;

/// One terminal showing the editor. A standalone editor has a single client, the default one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ClientId(pub u32);

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

tokio::task_local! {
    static TASK: Option<ClientId>;
}

thread_local! {
    static THREAD: Cell<Option<ClientId>> = const { Cell::new(None) };
}

/// The client the running work is for: the one its task or scope runs as, or else the one set
/// for this thread with [`set_current`].
pub fn current() -> Option<ClientId> {
    TASK.try_with(|client| *client)
        .unwrap_or_else(|_| THREAD.with(Cell::get))
}

/// Sets the client this thread's work is for, outside any [`scope`]. The main thread sets it as
/// it switches between clients.
pub fn set_current(client: Option<ClientId>) {
    THREAD.with(|current| current.set(client));
}

/// Runs `f` as `client`.
pub fn sync_scope<R>(client: Option<ClientId>, f: impl FnOnce() -> R) -> R {
    TASK.sync_scope(client, f)
}

/// Runs `future` as `client`.
pub fn scope<F: Future>(client: Option<ClientId>, future: F) -> impl Future<Output = F::Output> {
    TASK.scope(client, future)
}

/// The current client's working directory and environment, for work handed off to another
/// thread to run in. The main thread has moved on to another client's by the time such work
/// reads them. Work for no client reads whatever they are then, as it did where it came from.
fn env_snapshot() -> Option<Snapshot> {
    current().map(|_| Snapshot::capture())
}

fn in_env<R>(env: &Option<Snapshot>, f: impl FnOnce() -> R) -> R {
    match env {
        Some(env) => env.enter(f),
        None => f(),
    }
}

/// Like [`tokio::spawn`], but the task runs as the current client, with its working directory
/// and environment as they are now.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let env = env_snapshot();
    tokio::spawn(scope(current(), async move {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| in_env(&env, || future.as_mut().poll(cx))).await
    }))
}

/// Like [`tokio::task::spawn_blocking`], but `f` runs as the current client, with its working
/// directory and environment as they are now.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let client = current();
    let env = env_snapshot();
    tokio::task::spawn_blocking(move || in_env(&env, || sync_scope(client, f)))
}

/// The sending half of a channel whose messages carry the client that sent them.
pub struct Sender<T>(mpsc::Sender<(Option<ClientId>, T)>);

pub type Receiver<T> = mpsc::Receiver<(Option<ClientId>, T)>;

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A bounded channel whose messages carry the client that sent them.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::channel(capacity);
    (Sender(tx), rx)
}

impl<T> Sender<T> {
    /// Sends `data` as the current client. Drops it if the receiver is gone.
    pub async fn send(&self, data: T) {
        let _ = self.0.send((current(), data)).await;
    }

    /// Sends `data` as the current client, dropping it if the channel stays full for longer than
    /// `timeout`.
    pub async fn send_timeout(&self, data: T, timeout: Duration) {
        let _ = self.0.send_timeout((current(), data), timeout).await;
    }
}

/// Sends `data` from synchronous code as the current client.
pub fn send_blocking<T>(tx: &Sender<T>, data: T) {
    // block_on has some overhead and in practice the channel should basically
    // never be full anyway so first try sending without blocking
    if let Err(TrySendError::Full(data)) = tx.0.try_send((current(), data)) {
        // set a timeout so that we just drop a message instead of freezing the editor in the worst case
        let _ = block_on(tx.0.send_timeout(data, Duration::from_millis(10)));
    }
}
