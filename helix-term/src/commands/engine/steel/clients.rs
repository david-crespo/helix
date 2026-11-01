//! The clients sharing the editor, as Steel sees them: the client code runs as, the clients
//! and their views, switching clients, and hooks for clients coming and going. Clients are
//! integers.

use helix_event::register_hook;
use helix_view::{ClientId, ViewId};
use steel::{
    rvals::{Custom, IntoSteelVal},
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    RootedSteelVal, SteelVal,
};

use super::{construct_callback, patch_callbacks, Context, CTX};
use crate::{
    events::{ClientAttached, ClientDetached},
    job,
};

pub(super) fn register_api(module: &mut BuiltInModule) {
    module
        .register_fn_with_ctx(CTX, "current-client", |cx: &mut Context| {
            cx.editor.active_client().0
        })
        .register_fn_with_ctx(CTX, "editor-clients", |cx: &mut Context| {
            let mut clients: Vec<_> = cx.editor.clients().map(|client| client.0).collect();
            clients.sort();
            clients
        })
        .register_fn_with_ctx(CTX, "client-views", client_views)
        .register_fn_with_ctx(CTX, "view->client", |cx: &mut Context, view: ViewId| {
            let tree = &cx.editor.tree;
            tree.contains(view).then(|| tree.client_of(view).0)
        })
        .register_fn_with_ctx(CTX, "set-current-client!", set_current_client)
        .register_fn_with_ctx(CTX, "restore-current-client!", restore_current_client);
}

fn existing(cx: &Context, client: u32) -> anyhow::Result<ClientId> {
    let client = ClientId(client);
    anyhow::ensure!(
        cx.editor.clients().any(|c| c == client),
        "there's no client {client}"
    );
    Ok(client)
}

fn client_views(cx: &mut Context, client: u32) -> anyhow::Result<Vec<ViewId>> {
    let client = existing(cx, client)?;
    Ok(cx.editor.tree.client_views(client))
}

/// Who Steel code runs as: the active client, whose layout and state it sees, and the client
/// its work is for, which is none for work no client caused.
#[derive(Clone, Copy, PartialEq)]
struct RunningAs {
    active: ClientId,
    current: Option<ClientId>,
}

impl Custom for RunningAs {}

impl RunningAs {
    fn now(cx: &Context) -> Self {
        Self {
            active: cx.editor.active_client(),
            current: helix_event::client::current(),
        }
    }
}

/// Switches to `running`, and returns who code ran as before.
fn run_as(cx: &mut Context, running: RunningAs) -> RunningAs {
    // Compositor callbacks queued so far go to the client that queued them.
    patch_callbacks(cx);
    let previous = RunningAs::now(cx);
    cx.editor.activate(running.active);
    helix_event::client::set_current(running.current);
    previous
}

/// Makes Steel code run as `client` until [`restore_current_client`] sets back what this
/// returns, which `with-client` does.
fn set_current_client(cx: &mut Context, client: u32) -> anyhow::Result<RunningAs> {
    let client = existing(cx, client)?;
    Ok(run_as(
        cx,
        RunningAs {
            active: client,
            current: Some(client),
        },
    ))
}

fn restore_current_client(cx: &mut Context, previous: RunningAs) {
    if cx.editor.clients().any(|client| client == previous.active) {
        run_as(cx, previous);
    }
}

/// Runs `f`, which runs Steel code that Ctrl-C can interrupt, and switches back afterwards to
/// who code ran as before. `with-client` does that from Steel, but not when its thunk is
/// interrupted: Steel then unwinds without calling `restore-current-client!`.
pub(super) fn switching_back<R>(cx: &mut Context, f: impl FnOnce(&mut Context) -> R) -> R {
    let previous = RunningAs::now(cx);
    let result = f(cx);
    if RunningAs::now(cx) != previous {
        restore_current_client(cx, previous);
    }
    result
}

pub(super) fn register_client_attached(
    generation: usize,
    rooted: RootedSteelVal,
) -> steel::UnRecoverableResult {
    register_hook!(move |event: &mut ClientAttached<'_>| {
        let client = event.client.0.into_steelval().unwrap();
        job::dispatch_blocking_jobs(construct_callback(
            generation,
            rooted.value().clone(),
            [client],
        ));
        Ok(())
    });
    Ok(SteelVal::Void).into()
}

pub(super) fn register_client_detached(
    generation: usize,
    rooted: RootedSteelVal,
) -> steel::UnRecoverableResult {
    register_hook!(move |event: &mut ClientDetached<'_>| {
        let client = event.client.0.into_steelval().unwrap();
        job::dispatch_blocking_jobs(construct_callback(
            generation,
            rooted.value().clone(),
            [client],
        ));
        Ok(())
    });
    Ok(SteelVal::Void).into()
}
