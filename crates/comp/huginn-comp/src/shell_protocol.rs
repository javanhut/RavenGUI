//! Server side of `raven_shell_v1`.
//!
//! Muninn can see its own surfaces and nothing else — an ordinary Wayland
//! client is deliberately blind to the rest of the session. This is the narrow,
//! explicit hole through which the compositor tells the shell what it needs to
//! draw a panel.
//!
//! Kept small on purpose. Every request added here is a permanent commitment:
//! the protocol is additive-only, so anything shipped has to keep working.

use raven_protocol::server::{
    raven_shell_manager_v1::{self, RavenShellManagerV1},
    raven_workspace_state_v1::{self, RavenWorkspaceStateV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ClientId,
};

use huginn_core::Space;

use crate::state::Huginn;

/// The workspace facts the protocol reports.
///
/// Compared against the last one sent so that unchanged state produces no
/// event — a panel that is told nothing changed will still redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WorkspaceSnapshot {
    pub count: u32,
    pub active: u32,
    /// Bit N set if workspace N holds at least one window.
    pub occupied: u32,
}

impl WorkspaceSnapshot {
    pub(crate) fn of(space: &Space) -> Self {
        // The bitmask caps at 32 workspaces. take(32) keeps a hypothetical
        // 33rd from silently shifting into oblivion via overflow.
        let occupied = space
            .workspaces()
            .iter()
            .take(32)
            .enumerate()
            .filter(|(_, ws)| !ws.is_empty())
            .fold(0u32, |mask, (i, _)| mask | (1 << i));

        Self {
            count: space.workspaces().len() as u32,
            active: space.active_index() as u32,
            occupied,
        }
    }
}

/// Clients observing workspace state.
#[derive(Debug, Default)]
pub(crate) struct RavenShellState {
    observers: Vec<RavenWorkspaceStateV1>,
    last_sent: Option<WorkspaceSnapshot>,
}

impl RavenShellState {
    pub(crate) fn new(dh: &DisplayHandle) -> Self {
        // NOTE: advertised to every client. The protocol says this global
        // belongs to privileged clients only; Huginn has no notion of
        // privilege yet, so any client can currently watch workspace state and
        // switch workspaces. Harmless on a single-user session, but it must be
        // gated (security-context, or a socket only the shell can reach)
        // before anything untrusted runs on this compositor.
        dh.create_global::<Huginn, RavenShellManagerV1, ()>(1, ());
        Self::default()
    }
}

impl Huginn {
    /// Send workspace state to every observer, if it changed.
    pub(crate) fn broadcast_workspaces(&mut self) {
        let snapshot = WorkspaceSnapshot::of(&self.space);
        if self.raven_shell.last_sent == Some(snapshot) {
            return;
        }
        self.raven_shell.last_sent = Some(snapshot);

        self.raven_shell.observers.retain(|o| o.is_alive());
        for observer in &self.raven_shell.observers {
            observer.state(snapshot.count, snapshot.active, snapshot.occupied);
        }
        if !self.raven_shell.observers.is_empty() {
            tracing::debug!(
                observers = self.raven_shell.observers.len(),
                active = snapshot.active,
                occupied = format!("{:0width$b}", snapshot.occupied, width = snapshot.count as usize),
                "workspace state sent"
            );
        }
    }
}

impl GlobalDispatch<RavenShellManagerV1, ()> for Huginn {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<RavenShellManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
        tracing::debug!("shell bound raven_shell_manager_v1");
    }
}

impl Dispatch<RavenShellManagerV1, ()> for Huginn {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &RavenShellManagerV1,
        request: raven_shell_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            raven_shell_manager_v1::Request::GetWorkspaceState { id } => {
                let observer = data_init.init(id, ());
                // Send the current state immediately, so the shell never has to
                // render a placeholder while it waits for the first change.
                let snapshot = WorkspaceSnapshot::of(&state.space);
                observer.state(snapshot.count, snapshot.active, snapshot.occupied);
                state.raven_shell.last_sent = Some(snapshot);
                state.raven_shell.observers.push(observer);
                tracing::debug!("shell is now observing workspace state");
            }
            raven_shell_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<RavenWorkspaceStateV1, ()> for Huginn {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &RavenWorkspaceStateV1,
        request: raven_workspace_state_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            raven_workspace_state_v1::Request::Activate { index } => {
                // activate_workspace rejects an out-of-range index rather than
                // clamping, exactly as the protocol promises.
                if state.space.activate_workspace(index as usize) {
                    tracing::debug!(index, "shell switched workspace");
                    state.arrange();
                    state.refresh_focus();
                    state.broadcast_workspaces();
                } else {
                    tracing::debug!(index, "shell asked for an invalid workspace; ignored");
                }
            }
            raven_workspace_state_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &RavenWorkspaceStateV1,
        _data: &(),
    ) {
        state.raven_shell.observers.retain(|o| o != resource);
    }
}
