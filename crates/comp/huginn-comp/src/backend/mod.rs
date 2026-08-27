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
/// and are additionally told which display(s) to connect to, what kind of
/// session this is, and where the session bus lives — three things huginn's own
/// environment cannot carry, because huginn is started before any of them
/// exist.
pub(crate) fn spawn(argv: &[String], socket: &str, x11_display: Option<u32>) {
    let Some((program, args)) = argv.split_first() else {
        return;
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
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let bus = std::path::Path::new(&runtime_dir).join("bus");
            if bus.exists() {
                command.env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", bus.display()),
                );
            }
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
        }
        Err(e) => tracing::warn!(?argv, error = %e, "spawn failed"),
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
fn reap(mut child: std::process::Child, program: &str) {
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
