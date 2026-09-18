//! Which fingerprint stack this machine has, and how to get one if it has none.
//!
//! There are two, and the order between them is the whole of this module.
//!
//! **`raven-fprintd`** is RavenLinux's own: one small daemon that talks to the
//! reader over usbfs, built from the RavenLinux tree by `imlazy dev` or by the
//! ISO's raven stage, and installed to `/usr/bin` with everything else out of
//! that crate. No `libfprint`, no `libusb`, no D-Bus — which is why it is
//! preferred whenever it is there.
//!
//! **`fprintd`** is the conventional one, and it comes from `rvn`: RavenLinux
//! shares Arch's package names, so `extra/fprintd` and `extra/libfprint`
//! install and work. It is a GLib D-Bus service over a C USB library, and on a
//! static-musl image it is the largest runtime dependency anything asks for —
//! but a machine whose fingerprint reader works is better than a machine whose
//! fingerprint reader is an architectural principle, and there are readers
//! `libfprint` drives that `raven-fprintd` does not.
//!
//! So: Raven's if the image built one, `rvn`'s if it did not, and a machine
//! with neither is told exactly what to install rather than left to find out
//! that nothing happens when it touches the sensor.
//!
//! # Nothing here installs anything on its own
//!
//! [`Stack::install_command`] returns the argv and stops. Installing a package
//! is a privileged, outward-facing change to somebody's machine that fetches
//! code over the network, and a desktop that did it because a panel was opened
//! would be a desktop that installs things nobody asked for. The caller shows
//! it, asks, and runs it — see [`Stack::install`] for the one that runs.

use std::path::Path;

/// Where `raven-fprintd` listens once it is running.
///
/// Root-only, and deliberately: it reports matches without being told whose
/// they are, so unlike the lock screen's verify socket it cannot be offered to
/// a session. Its *existence* is still readable, which is all this needs.
pub const NATIVE_SOCKET: &str = "/run/raven-fprint/sensor.sock";

/// The daemon itself, for a machine where it is installed but not started.
pub const NATIVE_BINARY: &str = "/usr/bin/raven-fprintd";

/// `fprintd`'s D-Bus activation file. The most reliable sign it is installed:
/// the daemon is normally not running, because it is bus-activated.
const FPRINTD_SERVICE: &str = "/usr/share/dbus-1/system-services/net.reactivated.Fprint.service";

/// The packages `rvn` would install. `libfprint` comes in as a dependency of
/// `fprintd`, and is named anyway so that a resolver which has changed its mind
/// about that still produces a working stack.
pub const RVN_PACKAGES: &[&str] = &["fprintd", "libfprint"];

/// What this machine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    /// `raven-fprintd`: RavenLinux built it. Preferred whenever present.
    Native,
    /// `fprintd`, from `rvn`. Used when Raven's own is not there.
    Fprintd,
    /// Neither. See [`Stack::install_command`].
    Missing,
}

impl Stack {
    /// Look for one.
    ///
    /// Four path tests and no processes started, no bus connected and no device
    /// opened: this is asked while a panel is being drawn, and the answer must
    /// not depend on a daemon deciding to reply. It reports what is *installed*
    /// rather than what is working, which is the question a caller offering to
    /// install something actually has.
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_under(Path::new("/"))
    }

    /// [`Self::detect`], rooted at `root`, so the rules above can be tested
    /// against a directory rather than against whatever the build machine
    /// happens to have installed.
    #[must_use]
    pub fn detect_under(root: &Path) -> Self {
        let has = |absolute: &str| root.join(absolute.trim_start_matches('/')).exists();
        if has(NATIVE_SOCKET) || has(NATIVE_BINARY) {
            return Self::Native;
        }
        if has(FPRINTD_SERVICE) {
            return Self::Fprintd;
        }
        Self::Missing
    }

    /// Whether a reader could be used at all.
    #[must_use]
    pub const fn present(self) -> bool {
        !matches!(self, Self::Missing)
    }

    /// What to say on screen about it.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Native => "Raven's own fingerprint daemon",
            Self::Fprintd => "fprintd",
            Self::Missing => "no fingerprint support installed",
        }
    }

    /// The command that would install a stack, or `None` when one is already
    /// here.
    ///
    /// `sudo` because `rvn install` writes into `/usr`, and `--yes` because the
    /// caller has already asked the person — a prompt on a terminal nobody is
    /// looking at is a hang, not a confirmation.
    ///
    /// Returned rather than run; see the module note.
    #[must_use]
    pub fn install_command(self) -> Option<Vec<&'static str>> {
        if self.present() {
            return None;
        }
        let mut argv = vec!["sudo", "rvn", "install", "--yes"];
        argv.extend_from_slice(RVN_PACKAGES);
        Some(argv)
    }

    /// One line to put in front of somebody before running it.
    ///
    /// Says what will happen and where it comes from. A dialog that said only
    /// "install fingerprint support?" would be asking permission for something
    /// it had not described.
    #[must_use]
    pub const fn install_prompt(self) -> &'static str {
        "Install fingerprint support? This downloads fprintd and libfprint \
         from the package repositories and needs an administrator password."
    }

    /// Run [`Self::install_command`] and wait for it.
    ///
    /// Blocks, fetches over the network and asks for a password on whatever
    /// terminal it inherits, so it belongs on a thread and behind an explicit
    /// yes — never on a frame loop and never on a panel opening. Returns the
    /// stack as it stands afterwards, which is [`Self::Missing`] again if the
    /// install did not take.
    ///
    /// # Errors
    ///
    /// If the command could not be started at all. A command that ran and
    /// failed is not an error here: the return value says what the machine has
    /// now, which is the thing the caller needs either way.
    pub fn install(self) -> std::io::Result<Self> {
        let Some(argv) = self.install_command() else {
            return Ok(self);
        };
        let status = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .status()?;
        if !status.success() {
            // Worth a line in the log and not worth an error: the detect below
            // is the honest answer about what is installed, whatever rvn's
            // exit code said.
            tracing_warn(&format!("`{}` exited {status}", argv.join(" ")));
        }
        Ok(Self::detect())
    }
}

/// A warning, without making this crate depend on a logging framework.
///
/// `raven-fprint` has no dependencies on purpose — see the crate note — and one
/// line of diagnostics is not worth the first of them. Both consumers capture
/// stderr into their own logs.
fn tracing_warn(message: &str) {
    eprintln!("raven-fprint: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch root with some of the marker files in it.
    fn root_with(paths: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "raven-fprint-stack-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for path in paths {
            let full = root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(&full, b"").expect("write");
        }
        std::fs::create_dir_all(&root).expect("mkdir root");
        root
    }

    #[test]
    fn a_machine_with_nothing_is_told_what_to_install() {
        let root = root_with(&[]);
        let stack = Stack::detect_under(&root);
        assert_eq!(stack, Stack::Missing);
        assert!(!stack.present());
        let argv = stack.install_command().expect("something to run");
        assert_eq!(&argv[..4], &["sudo", "rvn", "install", "--yes"]);
        assert!(argv.contains(&"fprintd"));
    }

    #[test]
    fn the_running_native_daemon_is_found() {
        let root = root_with(&[NATIVE_SOCKET]);
        assert_eq!(Stack::detect_under(&root), Stack::Native);
    }

    /// Installed but not started still counts. The socket only exists while
    /// the daemon is up, and a panel that offered to install a second stack
    /// because a service had not been started yet would be a panel that
    /// installs fprintd onto a machine that already has a driver.
    #[test]
    fn the_native_daemon_counts_before_it_has_started() {
        let root = root_with(&[NATIVE_BINARY]);
        assert_eq!(Stack::detect_under(&root), Stack::Native);
    }

    #[test]
    fn fprintd_is_found_by_its_activation_file() {
        let root = root_with(&[FPRINTD_SERVICE]);
        assert_eq!(Stack::detect_under(&root), Stack::Fprintd);
    }

    /// The order that matters: RavenLinux's own wins. A machine that has both
    /// should not be driving the reader through a GLib service.
    #[test]
    fn ravens_own_wins_over_fprintd() {
        let root = root_with(&[NATIVE_BINARY, FPRINTD_SERVICE]);
        assert_eq!(Stack::detect_under(&root), Stack::Native);
    }

    /// Nothing is offered to a machine that already has a stack -- neither
    /// stack should ever be installed over the other.
    #[test]
    fn a_machine_that_has_one_is_offered_nothing() {
        assert_eq!(Stack::Native.install_command(), None);
        assert_eq!(Stack::Fprintd.install_command(), None);
    }

    /// The prompt has to say what it is going to do, or the yes it collects
    /// is not consent to anything in particular.
    #[test]
    fn the_prompt_names_what_it_installs_and_what_it_will_ask_for() {
        let prompt = Stack::Missing.install_prompt();
        assert!(prompt.contains("fprintd"));
        assert!(prompt.contains("password"));
    }

    #[test]
    fn every_stack_has_something_to_show() {
        for stack in [Stack::Native, Stack::Fprintd, Stack::Missing] {
            assert!(!stack.describe().is_empty());
        }
    }
}
