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
mod linux {
    use huginn_core::{Space, geometry::Rect};

    /// Which backend to drive.
    ///
    /// Having both from the start is the difference between a five-second edit
    /// loop and a reboot. `winit` runs the whole compositor inside a window on
    /// an existing desktop session, which is where essentially all development
    /// happens; `udev` is the real thing on a TTY.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Backend {
        Winit,
        Udev,
    }

    impl Backend {
        /// Pick a backend from `--backend`, falling back to autodetection.
        ///
        /// Running inside an existing session almost always means development,
        /// so an inherited `WAYLAND_DISPLAY` selects the nested backend.
        pub(crate) fn detect(args: &[String]) -> Self {
            match args.iter().position(|a| a == "--backend") {
                Some(i) => match args.get(i + 1).map(String::as_str) {
                    Some("udev") => Self::Udev,
                    _ => Self::Winit,
                },
                None if std::env::var_os("WAYLAND_DISPLAY").is_some() => Self::Winit,
                None => Self::Udev,
            }
        }
    }

    pub(crate) fn run(backend: Backend) {
        tracing::info!(?backend, "starting huginn");

        // Placeholder geometry until the backend reports real outputs.
        let mut space = Space::new(Rect::from_xywh(0, 0, 1920, 1080));
        tracing::info!(
            workspaces = space.workspaces().len(),
            layout = space.active_workspace().layout().name(),
            "window-management state initialised"
        );

        tracing::warn!("no backend implemented yet; exiting");
        let _ = space.arrange();
    }
}

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
        linux::run(linux::Backend::detect(&args));
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
