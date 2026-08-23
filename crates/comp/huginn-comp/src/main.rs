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
mod backend;
#[cfg(target_os = "linux")]
mod state;

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
            backend::Backend::Udev => {
                tracing::error!(
                    "the udev backend is not implemented yet. It also cannot be driven \
                     over ssh: logind grants DRM master only to the active session on a \
                     seat, and an ssh login has no seat."
                );
                std::process::exit(1);
            }
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
