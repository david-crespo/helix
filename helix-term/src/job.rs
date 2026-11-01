use helix_event::client::{self, ClientId};
use helix_event::status::StatusMessage;
use helix_event::{runtime_local, send_blocking};
use helix_view::Editor;
use once_cell::sync::OnceCell;

use crate::compositor::Compositor;

use futures_util::future::LocalBoxFuture;
use futures_util::future::{BoxFuture, Future, FutureExt};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};

pub type EditorCompositorJobsCallback =
    Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut Jobs) + Send>;
pub type EditorCompositorCallback = Box<dyn FnOnce(&mut Editor, &mut Compositor) + Send>;
pub type EditorCallback = Box<dyn FnOnce(&mut Editor) + Send>;
pub type EditorCallbackFollowup = Box<dyn FnOnce(&mut Editor) -> Option<Job> + Send>;

pub type ThreadLocalEditorCompositorCallback =
    Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut Jobs)>;

runtime_local! {
    /// Callbacks, with the client each runs as: the one current where it was dispatched. See
    /// [`helix_event::client`].
    static JOB_QUEUE: OnceCell<helix_event::Sender<Callback>> = OnceCell::new();
}

pub async fn dispatch_callback(job: Callback) {
    JOB_QUEUE.wait().send(job).await
}

pub async fn dispatch(job: impl FnOnce(&mut Editor, &mut Compositor) + Send + 'static) {
    dispatch_callback(Callback::EditorCompositor(Box::new(job))).await
}

pub fn dispatch_blocking(job: impl FnOnce(&mut Editor, &mut Compositor) + Send + 'static) {
    send_blocking(JOB_QUEUE.wait(), Callback::EditorCompositor(Box::new(job)))
}

pub fn dispatch_blocking_jobs(
    job: impl FnOnce(&mut Editor, &mut Compositor, &mut Jobs) + Send + 'static,
) {
    send_blocking(
        JOB_QUEUE.wait(),
        Callback::EditorCompositorJobs(Box::new(job)),
    )
}

pub enum Callback {
    EditorCompositorJobs(EditorCompositorJobsCallback),
    EditorCompositor(EditorCompositorCallback),
    Editor(EditorCallback),
    Followup(EditorCallbackFollowup),
}

pub type JobFuture = BoxFuture<'static, anyhow::Result<Option<Callback>>>;

/// A job that has to finish before exiting, with the client it runs as. It resolves to that
/// client and the job's result.
pub struct ClientJob {
    client: Option<ClientId>,
    future: JobFuture,
}

impl ClientJob {
    fn new(client: Option<ClientId>, future: JobFuture) -> Self {
        Self {
            client,
            future: client::scope(client, future).boxed(),
        }
    }
}

impl Future for ClientJob {
    type Output = (Option<ClientId>, anyhow::Result<Option<Callback>>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let client = self.client;
        self.future.poll_unpin(cx).map(|result| (client, result))
    }
}

pub struct Job {
    pub future: BoxFuture<'static, anyhow::Result<Option<Callback>>>,
    /// Do we need to wait for this job to finish before exiting?
    pub wait: bool,
}

pub type ThreadLocalJob = LocalBoxFuture<
    'static,
    (
        Option<ClientId>,
        anyhow::Result<Option<ThreadLocalEditorCompositorCallback>>,
    ),
>;

pub struct Jobs {
    /// jobs the ones that need to complete before we exit.
    pub wait_futures: FuturesUnordered<ClientJob>,
    pub local_futures: FuturesUnordered<ThreadLocalJob>,
    pub callbacks: client::Receiver<Callback>,
    pub status_messages: client::Receiver<StatusMessage>,
}

impl Job {
    pub fn new<F: Future<Output = anyhow::Result<()>> + Send + 'static>(f: F) -> Self {
        Self {
            future: f.map(|r| r.map(|()| None)).boxed(),
            wait: false,
        }
    }

    pub fn with_callback<F: Future<Output = anyhow::Result<Callback>> + Send + 'static>(
        f: F,
    ) -> Self {
        Self {
            future: f.map(|r| r.map(Some)).boxed(),
            wait: false,
        }
    }

    pub fn wait_before_exiting(mut self) -> Self {
        self.wait = true;
        self
    }
}

impl Jobs {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let (tx, rx) = client::channel(1024);
        let _ = JOB_QUEUE.set(tx);
        let status_messages = helix_event::status::setup();
        Self {
            wait_futures: FuturesUnordered::new(),
            local_futures: FuturesUnordered::new(),
            callbacks: rx,
            status_messages,
        }
    }

    pub fn spawn<F: Future<Output = anyhow::Result<()>> + Send + 'static>(&mut self, f: F) {
        self.add(Job::new(f));
    }

    pub fn callback<F: Future<Output = anyhow::Result<Callback>> + Send + 'static>(
        &mut self,
        f: F,
    ) {
        self.add(Job::with_callback(f));
    }

    pub fn local_callback<
        F: Future<Output = anyhow::Result<ThreadLocalEditorCompositorCallback>> + 'static,
    >(
        &mut self,
        f: F,
    ) {
        let client = client::current();
        self.local_futures
            .push(f.map(move |r| (client, r.map(Some))).boxed_local());
    }

    pub fn handle_callback(
        &mut self,
        editor: &mut Editor,
        compositor: &mut Compositor,
        call: anyhow::Result<Option<Callback>>,
    ) -> Option<Job> {
        match call {
            Ok(None) => None,
            Ok(Some(call)) => match call {
                Callback::EditorCompositorJobs(call) => {
                    call(editor, compositor, self);
                    None
                }
                Callback::EditorCompositor(call) => {
                    call(editor, compositor);
                    None
                }
                Callback::Editor(call) => {
                    call(editor);
                    None
                }
                Callback::Followup(call) => call(editor),
            },
            Err(e) => {
                editor.set_error(format!("Async job failed: {}", e));
                None
            }
        }
    }

    pub fn handle_local_callback(
        &mut self,
        editor: &mut Editor,
        compositor: &mut Compositor,
        call: anyhow::Result<Option<ThreadLocalEditorCompositorCallback>>,
    ) {
        match call {
            Ok(None) => {}
            Ok(Some(call)) => call(editor, compositor, self),
            Err(e) => {
                editor.set_error(format!("Sync job failed: {}", e));
            }
        }
    }

    /// Adds a job that runs as the current client.
    pub fn add(&self, j: Job) {
        if j.wait {
            self.wait_futures
                .push(ClientJob::new(client::current(), j.future));
        } else {
            client::spawn(async move {
                match j.future.await {
                    Ok(Some(cb)) => dispatch_callback(cb).await,
                    Ok(None) => (),
                    Err(err) => helix_event::status::report(err).await,
                }
            });
        }
    }

    /// Blocks until the jobs that need to be waited on are done, except those of attached clients
    /// other than the active one, which finish in the event loop as usual. `compositor` is the
    /// active client's. Each callback runs as its job's client.
    pub async fn finish(
        &mut self,
        editor: &mut Editor,
        mut compositor: Option<&mut Compositor>,
    ) -> anyhow::Result<()> {
        log::debug!("waiting on jobs...");
        let active = editor.active_client();
        let attached: Vec<_> = editor.clients().filter(|&c| c != active).collect();
        let (others, mut wait_futures): (FuturesUnordered<_>, _) =
            std::mem::take(&mut self.wait_futures)
                .into_iter()
                .partition(|job| job.client.is_some_and(|c| attached.contains(&c)));
        self.wait_futures = others;

        while let (Some((client, job)), tail) = wait_futures.into_future().await {
            wait_futures = tail;
            let callback = match job {
                Ok(Some(callback)) => callback,
                Ok(None) => continue,
                Err(e) => {
                    self.wait_futures.extend(wait_futures);
                    return Err(e);
                }
            };
            // A job of a client that has gone can't reach its UI.
            let compositor = compositor
                .as_deref_mut()
                .filter(|_| client.is_none_or(|c| c == active));
            let job = client::sync_scope(client, || match callback {
                Callback::EditorCompositor(call) if compositor.is_some() => {
                    call(editor, compositor.unwrap());
                    None
                }
                Callback::Editor(call) => {
                    call(editor);
                    None
                }
                Callback::Followup(call) => call(editor),

                // skip callbacks for which we don't have the necessary references
                _ => None,
            });
            if let Some(job) = job.filter(|job| job.wait) {
                wait_futures.push(ClientJob::new(client, job.future));
            }
        }

        Ok(())
    }
}
