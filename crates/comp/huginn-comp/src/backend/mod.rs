//! Backends: the two ways Huginn can reach a screen.

pub(crate) mod chord;
pub(crate) mod gpu;
pub(crate) mod gpu_class;
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
            Some(i) => Self::named(args.get(i + 1).map(String::as_str)),
            None if std::env::var_os("WAYLAND_DISPLAY").is_some() => Self::Winit,
            None => Self::Udev,
        }
    }

    /// The backend `--backend` named, saying so when it named nothing known.
    ///
    /// A value that is not understood still falls back to `winit`, because a
    /// compositor in a window is the recoverable end of getting this wrong --
    /// but silently is how `--backend udevv` on a TTY becomes a session that
    /// looks like it never started.
    fn named(value: Option<&str>) -> Self {
        match value {
            Some("udev") => Self::Udev,
            Some("winit") => Self::Winit,
            Some(other) => {
                tracing::warn!(value = other, "unrecognised --backend; using winit");
                Self::Winit
            }
            None => {
                tracing::warn!("--backend was given no value; using winit");
                Self::Winit
            }
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
/// and are additionally told which display(s) to connect to, what kind of
/// session this is, and where the session bus lives — three things huginn's own
/// environment cannot carry, because huginn is started before any of them
/// exist.
/// Returns whether the process actually started.
///
/// Every caller but one throws the answer away -- a launcher entry that will
/// not run is a message in the log and nothing more. The exception is the lock
/// screen, where "did it start" decides whether the compositor is about to
/// blank a screen that something can get past.
pub(crate) fn spawn(argv: &[String], socket: &str, x11_display: Option<u32>) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut command = std::process::Command::new(program);
    command.args(args).env("WAYLAND_DISPLAY", socket);
    // For the toolkits that ask what kind of session this is rather than
    // looking for WAYLAND_DISPLAY. Chromium is the one that matters: its Ozone
    // platform defaults to X11 and only chooses Wayland when the platform hint
    // resolves, which is decided by XDG_SESSION_TYPE. Without it a browser
    // launched from the dock or the launcher exits immediately with "Missing X
    // server or $DISPLAY" whenever XWayland is not up.
    command.env("XDG_SESSION_TYPE", "wayland");
    // Same reasoning, for the bus. GLib finds the session bus at the well-known
    // path on its own when DBUS_SESSION_BUS_ADDRESS is unset; libdbus, which
    // Chromium uses, autolaunches instead and fails. A browser with no session
    // bus cannot call org.freedesktop.FileManager1, so "show in folder" on a
    // download reaches no file manager and silently does nothing.
    //
    // Only when absent: a session that already set it chose that address, and
    // the well-known path is a fallback rather than an override.
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        && let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
    {
        let bus = std::path::Path::new(&runtime_dir).join("bus");
        if bus.exists() {
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus.display()),
            );
        }
    }
    // Toolkits pick Wayland when both are set, so this only decides where the
    // X11-only ones connect. Absent until XWayland signals ready, which is
    // deliberate: a child that inherits a DISPLAY pointing at an unmanaged X
    // server maps windows nobody will ever lay out.
    if let Some(display) = x11_display {
        command.env("DISPLAY", format!(":{display}"));
    }
    match command.spawn() {
        Ok(child) => {
            tracing::info!(?argv, "spawned");
            reap(child, program);
            true
        }
        Err(e) => {
            tracing::warn!(?argv, error = %e, "spawn failed");
            false
        }
    }
}

/// Tell the session bus which display D-Bus-activated services should use.
///
/// `spawn` covers what huginn starts itself; this covers what the bus starts on
/// demand. The session bus is up before huginn is, so its activation
/// environment has no WAYLAND_DISPLAY -- and every service it activates
/// inherits that. The one that bites is the portal backend: a browser's
/// Open/Upload dialog goes through xdg-desktop-portal to
/// `ravenfilemanager --portal`, which, started without a display, cannot open a
/// window and never answers. The picker simply never appears. The GTK backend
/// crashes outright for the same reason.
///
/// This is the call `dbus-update-activation-environment` makes. It only affects
/// services activated from now on, which is why it runs as soon as the socket
/// exists, before anything could have asked for a portal. It runs again once
/// XWayland is up, to add DISPLAY.
///
/// On a thread: it is a blocking round-trip to the bus, and the compositor must
/// not stall on a bus that is slow or missing. Failure is logged, not fatal --
/// the desktop works without it, only activated services go without a display.
pub(crate) fn publish_activation_environment(socket: &str, x11_display: Option<u32>) {
    let mut vars = vec![
        ("WAYLAND_DISPLAY".to_string(), socket.to_string()),
        ("XDG_SESSION_TYPE".to_string(), "wayland".to_string()),
    ];
    // Passed through rather than invented: the session script decides these.
    // Portals choose their backend by XDG_CURRENT_DESKTOP, and GTK_USE_PORTAL
    // makes a GTK3 program the bus starts use the portal dialogs, like one
    // started from the launcher.
    for name in ["XDG_CURRENT_DESKTOP", "GTK_USE_PORTAL"] {
        if let Ok(value) = std::env::var(name) {
            vars.push((name.to_string(), value));
        }
    }
    if let Some(display) = x11_display {
        vars.push(("DISPLAY".to_string(), format!(":{display}")));
    }

    let started = std::thread::Builder::new()
        .name("huginn-dbus-env".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let result = zbus::blocking::Connection::session().and_then(|bus| {
                let env: std::collections::HashMap<&str, &str> =
                    vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                bus.call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "UpdateActivationEnvironment",
                    &(env,),
                )
                .map(drop)
            });
            match result {
                Ok(()) => tracing::info!(?vars, "published activation environment"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "cannot update the D-Bus activation environment; \
                     bus-activated services such as portals will have no display"
                ),
            }
        });
    if let Err(e) = started {
        tracing::warn!(error = %e, "no thread to publish the activation environment");
    }
}

/// Wait for `child` on a thread of its own, so it does not become a zombie.
///
/// Nothing used to wait for these at all. `Child`'s `Drop` deliberately does
/// not reap -- so every application huginn started stayed in the process table
/// as a `<defunct>` entry from the moment it exited until the compositor did.
/// One leaked PID per launch is the small half of the cost. The large half is
/// that it hides failures: `ravencanvasd` died a tenth of a second into every
/// boot, and because nothing reaped it and nothing restarted it, the only
/// evidence on a running machine was a `Z` in `ps` under huginn.
///
/// A blocked thread rather than either alternative:
///
///   * `SIGCHLD` set to `SIG_IGN` has the kernel reap everything, but it is
///     process-wide, and smithay's XWayland integration keeps and waits on a
///     child of its own -- it would start getting ECHILD for a process it is
///     responsible for.
///   * A calloop timer sweeping `try_wait` would poll forever for something
///     that happens a handful of times a session, on a compositor whose idle
///     cost is meant to be nothing.
///
/// The thread is blocked in `waitpid` rather than spinning, and it ends when
/// the application does.
pub(crate) fn reap(mut child: std::process::Child, program: &str) {
    // Owned separately rather than shadowing: the closure takes this one, and
    // the failure branch below still needs the caller's.
    let name = program.to_string();
    let started = std::thread::Builder::new()
        // Linux caps a thread name at 15 bytes; a per-program name would be
        // truncated into something less useful than the constant.
        .name("huginn-reap".to_string())
        .stack_size(64 * 1024)
        .spawn(move || match child.wait() {
            Ok(status) => tracing::debug!(program = %name, ?status, "child exited"),
            Err(e) => tracing::warn!(program = %name, error = %e, "cannot wait for child"),
        });

    if let Err(e) = started {
        // Not fatal, and not worth refusing to launch things over: the child is
        // already running and will simply linger as a zombie, which is what
        // every launch did before this existed.
        tracing::warn!(program, error = %e, "no thread to reap the child; it will linger");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the kernel still has a process table entry for `pid`.
    ///
    /// A reaped child leaves nothing behind, so the directory disappearing is
    /// the property under test. An unreaped one stays as a zombie and keeps it.
    fn present(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn an_explicit_backend_is_taken_whatever_the_environment_says() {
        assert_eq!(
            Backend::detect(&argv(&["huginn", "--backend", "udev"])),
            Backend::Udev
        );
        assert_eq!(
            Backend::detect(&argv(&["huginn", "--backend", "winit"])),
            Backend::Winit
        );
    }

    /// The regression behind this: `--help` was not a flag huginn knew, so it
    /// fell through to autodetection, found an inherited `WAYLAND_DISPLAY` and
    /// started a nested compositor in a window instead of printing anything.
    /// `main` answers `--help` before it ever gets here, and this pins the
    /// other half -- that an unknown value is not quietly read as a backend.
    #[test]
    fn an_unrecognised_backend_value_falls_back_to_winit() {
        assert_eq!(Backend::named(Some("udevv")), Backend::Winit);
        assert_eq!(Backend::named(Some("--help")), Backend::Winit);
        assert_eq!(Backend::named(None), Backend::Winit);
    }

    #[test]
    fn the_backend_names_are_exactly_the_two_that_exist() {
        assert_eq!(Backend::named(Some("udev")), Backend::Udev);
        assert_eq!(Backend::named(Some("winit")), Backend::Winit);
    }

    #[test]
    fn a_spawned_child_does_not_stay_a_zombie() {
        // The regression: huginn spawned and forgot, so `ps` filled up with
        // `<defunct>` entries under the compositor as applications were closed.
        let child = std::process::Command::new("/bin/true")
            .spawn()
            .expect("/bin/true should be spawnable");
        let pid = child.id();

        reap(child, "/bin/true");

        // Generous: this is waiting on a thread to be scheduled and a process
        // to exit, not measuring either.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while present(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            !present(pid),
            "pid {pid} was never reaped; it is still in the process table"
        );
    }
}
