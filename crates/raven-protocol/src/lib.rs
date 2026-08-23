//! The `raven_shell_v1` protocol.
//!
//! Everything `muninn` needs that no standard protocol provides: window lists
//! for the taskbar, workspace state for the panel, and the privileged requests
//! behind the launcher. Standard protocols cover the rest — panels and the
//! wallpaper use `wlr-layer-shell`, the lock screen uses `ext-session-lock-v1`.
//!
//! # Stability
//!
//! This crate is a public API contract even while it lives in the monorepo.
//! Treat every published interface as frozen: add new requests and events
//! rather than changing existing ones, and bump the interface version in
//! `protocols/raven-shell-v1.xml` when you do. Holding that line is what keeps
//! splitting this into its own repository a mechanical operation later.

#[cfg(feature = "server")]
pub mod server {
    //! Compositor-side bindings, implemented by `huginn-comp`.
    use wayland_server as _;
}

#[cfg(feature = "client")]
pub mod client {
    //! Shell-side bindings, consumed by `muninn`.
    use wayland_client as _;
}
