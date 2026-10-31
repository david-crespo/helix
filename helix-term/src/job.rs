use helix_event::status::StatusMessage;
use helix_event::{runtime_local, send_blocking};
use helix_view::{ClientId, Editor};
use once_cell::sync::OnceCell;

use crate::compositor::Compositor;

use futures_util::future::LocalBoxFuture;
use futures_util::future::{BoxFuture, Future, FutureExt};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc::{channel, Receiver, Sender};

pub type EditorCompositorJobsCallback =
    Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut Jobs) + Send>;
pub type EditorCompositorCallback = Box<dyn FnOnce(&mut Editor, &mut Compositor) + Send>;
pub type EditorCallback = Box<dyn FnOnce(&mut Editor) + Send>;
pub type EditorCallbackFollowup = Box<dyn FnOnce(&mut Editor) -> Option<Job> + Send>;

pub type ThreadLocalEditorCompositorCallback =
    Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut Jobs)>;

runtime_local! {
    static JOB_QUEUE: OnceCell<Sender<(Option<ClientId>, Callback)>> = OnceCell::new();
}

tokio::task_local! {
    /// The client a job runs as: the one that was active when the job was added.
    static CLIENT: ClientId;
}

/// Callbacks dispatched from a job run as the job's client. Others, such as those from hooks
/// and threads, run as whichever client last had input.
fn job_client() -> Option<ClientId> {
    CLIENT.try_with(|client| *client).ok()
}

pub async fn dispatch_callback(job: Callback) {
    let _ = JOB_QUEUE.wait().send((job_client(), job)).await;
}

pub async fn dispatch(job: impl FnOnce(&mut Editor, &mut Compositor) + Send + 'static) {
    dispatch_callback(Callback::EditorCompositor(Box::new(job))).await
}

pub fn dispatch_blocking(job: impl FnOnce(&mut Editor, &mut Compositor) + Send + 'static) {
    let jobs = JOB_QUEUE.wait();
    send_blocking(
        jobs,
        (job_client(), Callback::EditorCompositor(Box::new(job))),
    )
}

pub fn dispatch_blocking_jobs(
    job: impl FnOnce(&mut Editor, &mut Compositor, &mut Jobs) + Send + 'static,
) {
    let jobs = JOB_QUEUE.wait();
    send_blocking(
        jobs,
        (job_client(), Callback::EditorCompositorJobs(Box::new(job))),
    )
}

pub enum Callback {
    EditorCompositorJobs(EditorCompositorJobsCallback),
    EditorCompositor(EditorCompositorCallback),
    Editor(EditorCallback),
    Followup(EditorCallbackFollowup),
}

pub type JobFuture = BoxFuture<'static, anyhow::Result<Option<Callback>>>;
/// A job's result, with the client it runs as.
pub type ClientJobFuture = BoxFuture<'static, (ClientId, anyhow::Result<Option<Callback>>)>;

pub struct Job {
    pub future: BoxFuture<'static, anyhow::Result<Option<Callback>>>,
    /// Do we need to wait for this job to finish before exiting?
    pub wait: bool,
}

pub type ThreadLocalJob = LocalBoxFuture<
    'static,
    (
        ClientId,
        anyhow::Result<Option<ThreadLocalEditorCompositorCallback>>,
    ),
>;

pub struct Jobs {
    /// jobs the ones that need to complete before we exit.
    pub wait_futures: FuturesUnordered<ClientJobFuture>,
    pub local_futures: FuturesUnordered<ThreadLocalJob>,
    pub callbacks: Receiver<(Option<ClientId>, Callback)>,
    pub status_messages: Receiver<StatusMessage>,
    /// The active client. Jobs added now run as this client.
    pub client: ClientId,
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
        let (tx, rx) = channel(1024);
        let _ = JOB_QUEUE.set(tx);
        let status_messages = helix_event::status::setup();
        Self {
            wait_futures: FuturesUnordered::new(),
            local_futures: FuturesUnordered::new(),
            callbacks: rx,
            status_messages,
            client: ClientId::default(),
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
        let client = self.client;
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

    pub fn add(&self, j: Job) {
        let client = self.client;
        if j.wait {
            self.wait_futures.push(
                CLIENT
                    .scope(client, j.future)
                    .map(move |r| (client, r))
                    .boxed(),
            );
        } else {
            tokio::spawn(CLIENT.scope(client, async move {
                match j.future.await {
                    Ok(Some(cb)) => dispatch_callback(cb).await,
                    Ok(None) => (),
                    Err(err) => helix_event::status::report(err).await,
                }
            }));
        }
    }

    /// Blocks until all the jobs that need to be waited on are done.
    pub async fn finish(
        &mut self,
        editor: &mut Editor,
        mut compositor: Option<&mut Compositor>,
    ) -> anyhow::Result<()> {
        log::debug!("waiting on jobs...");
        let mut wait_futures = std::mem::take(&mut self.wait_futures);

        while let (Some((client, job)), tail) = wait_futures.into_future().await {
            match job {
                Ok(callback) => {
                    wait_futures = tail;

                    if let Some(callback) = callback {
                        // clippy doesn't realize this is an error without the derefs
                        #[allow(clippy::needless_option_as_deref)]
                        if let Some(job) = match callback {
                            Callback::EditorCompositor(call) if compositor.is_some() => {
                                call(editor, compositor.as_deref_mut().unwrap());
                                None
                            }
                            Callback::Editor(call) => {
                                call(editor);
                                None
                            }
                            Callback::Followup(call) => call(editor),

                            // skip callbacks for which we don't have the necessary references
                            _ => None,
                        } {
                            if job.wait {
                                wait_futures.push(job.future.map(move |r| (client, r)).boxed());
                            }
                        }
                    }
                }
                Err(e) => {
                    self.wait_futures = tail;
                    return Err(e);
                }
            }
        }

        Ok(())
    }
}
