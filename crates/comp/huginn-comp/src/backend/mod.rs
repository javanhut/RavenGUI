//! Backends: the two ways Huginn can reach a screen.

pub(crate) mod chord;
pub(crate) mod input;
pub(crate) mod keymap;
pub(crate) mod udev;
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

/// Run `argv` as a desktop application on `socket`.
///
/// A free function rather than a method so it borrows nothing but its
/// arguments: both backends call it while holding a mutable borrow of the
/// compositor state, which a method on `&self` would conflict with.
///
/// The argv comes from `Entry::argv`, which has already split it and stripped
/// field codes — it never goes near a shell. Children inherit the environment
/// and are additionally told which display to connect to.
pub(crate) fn spawn(argv: &[String], socket: &str) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    match std::process::Command::new(program)
        .args(args)
        .env("WAYLAND_DISPLAY", socket)
        .spawn()
    {
        Ok(_) => tracing::info!(?argv, "spawned"),
        Err(e) => tracing::warn!(?argv, error = %e, "spawn failed"),
    }
}
