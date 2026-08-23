//! The `raven_shell_v1` protocol.
//!
//! Everything Muninn needs that no standard protocol provides. Standard
//! protocols cover the rest — panels and the wallpaper use `wlr-layer-shell`,
//! the lock screen uses `ext-session-lock-v1`.
//!
//! # Stability
//!
//! This crate is a public API contract even while it lives in the monorepo.
//! Treat every published interface as frozen: add new requests and events
//! rather than changing existing ones, and bump the interface version in
//! `protocols/raven-shell-v1.xml` when you do. Holding that line is what keeps
//! splitting this into its own repository a mechanical operation later.
//!
//! # Layout
//!
//! The two sides are behind features so neither can reach for the other's
//! types. Huginn takes `server`, Muninn takes `client`; a shell bug then
//! cannot compile against a compositor-only interface.

#[cfg(feature = "server")]
pub mod server {
    //! Compositor-side bindings, implemented by `huginn-comp`.
    use wayland_server;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("../../protocols/raven-shell-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("../../protocols/raven-shell-v1.xml");
}

#[cfg(feature = "client")]
pub mod client {
    //! Shell-side bindings, consumed by `muninn`.
    use wayland_client;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("../../protocols/raven-shell-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocols/raven-shell-v1.xml");
}
