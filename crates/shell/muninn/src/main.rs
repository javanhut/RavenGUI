//! `muninn` — the RavenLinux desktop shell.
//!
//! Muninn is the second of Odin's ravens — *memory*. It is the half of the pair
//! you actually see: the panel, the launcher, the notifications.
//!
//! A plain Wayland client, not part of the compositor. It draws its surfaces
//! through `wlr-layer-shell` and asks the compositor for window and workspace
//! state over `raven_shell_v1`.
//!
//! Running out-of-process is a deliberate trade. GNOME embeds its shell in
//! Mutter and gets synchronous access to window state in exchange; the cost is
//! a single failure domain, where a panel bug takes down every open
//! application with it. Here a crashed `muninn` loses the panel and nothing
//! else, and it can be restarted — or rebuilt and swapped — without disturbing
//! a running session.

#[cfg(target_os = "linux")]
mod panel;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muninn=debug".into()),
        )
        .init();

    #[cfg(target_os = "linux")]
    {
        if let Err(e) = panel::run() {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::error!("muninn needs a Wayland session; build it on the RavenLinux host");
        std::process::exit(1);
    }
}
