//! Backends: the two ways Huginn can reach a screen.

pub(crate) mod winit;

/// Which backend to drive.
///
/// Having both from the start is the difference between a five-second edit loop
/// and a reboot. `winit` runs the whole compositor inside a window on an
/// existing desktop session, which is where essentially all development
/// happens; `udev` is the real thing on a TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Winit,
    Udev,
}

impl Backend {
    /// Pick a backend from `--backend`, falling back to autodetection.
    ///
    /// Running inside an existing session almost always means development, so
    /// an inherited `WAYLAND_DISPLAY` selects the nested backend.
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
