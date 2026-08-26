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

/// What to tell the protocols about an output's scale.
///
/// Two numbers, on purpose. `advertised_integer` is what `wl_output` says, and
/// every client renders at it. `fractional` is what the compositor lays the
/// desktop out at and what `DrmCompositor` composes surfaces with, and it also
/// reaches `xdg_output`, whose logical size has to agree with the desktop the
/// core actually laid out. See `huginn_core::scale`.
pub(crate) fn advertise(scale: huginn_core::scale::OutputScale) -> smithay::output::Scale {
    smithay::output::Scale::Custom {
        advertised_integer: scale.advertised as i32,
        fractional: scale.fractional(),
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
/// and are additionally told which display(s) to connect to.
pub(crate) fn spawn(argv: &[String], socket: &str, x11_display: Option<u32>) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let mut command = std::process::Command::new(program);
    command.args(args).env("WAYLAND_DISPLAY", socket);
    // Toolkits pick Wayland when both are set, so this only decides where the
    // X11-only ones connect. Absent until XWayland signals ready, which is
    // deliberate: a child that inherits a DISPLAY pointing at an unmanaged X
    // server maps windows nobody will ever lay out.
    if let Some(display) = x11_display {
        command.env("DISPLAY", format!(":{display}"));
    }
    match command.spawn()
    {
        Ok(_) => tracing::info!(?argv, "spawned"),
        Err(e) => tracing::warn!(?argv, error = %e, "spawn failed"),
    }
}
