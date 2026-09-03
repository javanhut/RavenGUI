//! The Huginn compositor.
//!
//! Huginn is one of Odin's two ravens — *thought*. It flies out over the world
//! at dawn and reports back what it saw. Its counterpart Muninn — *memory* — is
//! the desktop shell that renders what Huginn reports.
//!
//! Window-management behaviour lives in `huginn-core`, which has no Wayland or
//! GPU dependency and is tested on its own. This binary is the part that cannot
//! be tested without hardware: session setup, the event loop, protocol
//! handlers, and rendering.

#[cfg(target_os = "linux")]
mod anim;
#[cfg(target_os = "linux")]
mod appwatch;
mod audio;
#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod bluetooth;
mod blur;
#[cfg(target_os = "linux")]
mod canvas;
mod configwatch;
#[cfg(target_os = "linux")]
mod decor;
#[cfg(target_os = "linux")]
mod desktop_config;
mod dmabuf;
#[cfg(target_os = "linux")]
mod dock;
mod fileindex;
#[cfg(target_os = "linux")]
mod gesture;
#[cfg(target_os = "linux")]
mod launcher;
#[cfg(target_os = "linux")]
mod motion;
#[cfg(target_os = "linux")]
mod overlay;
mod overview;
mod pinned;
mod pins;
#[cfg(target_os = "linux")]
mod pointer;
#[cfg(target_os = "linux")]
mod popup;
#[cfg(target_os = "linux")]
mod render;
#[cfg(target_os = "linux")]
mod screenshot;
#[cfg(target_os = "linux")]
mod settings;
#[cfg(target_os = "linux")]
mod shell_protocol;
#[cfg(target_os = "linux")]
mod sleep;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod text;
#[cfg(target_os = "linux")]
mod theme;
#[cfg(target_os = "linux")]
mod wallpaper;
#[cfg(target_os = "linux")]
mod window;
#[cfg(target_os = "linux")]
mod xwayland;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "huginn=debug,smithay=warn".into()),
        )
        .init();

    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().collect();
        let chosen = backend::Backend::detect(&args);
        tracing::info!(backend = ?chosen, "starting huginn");

        let result = match chosen {
            backend::Backend::Winit => backend::winit::run(),
            backend::Backend::Udev => backend::udev::run(),
        };

        if let Err(e) = result {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::error!(
            "huginn targets Linux only: it needs DRM/KMS, libinput and libseat, \
             which have no macOS equivalent. Build it on the RavenLinux host. \
             huginn-core, raven-config and raven-protocol build here — run \
             `cargo test -p huginn-core` for window-management work."
        );
        std::process::exit(1);
    }
}
