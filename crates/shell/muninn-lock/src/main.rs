//! `muninn-lock` — the RavenLinux lock screen.
//!
//! Separate from `muninn` for one reason: `ext-session-lock-v1` specifies that
//! if the locking client dies while the session is locked, the compositor keeps
//! the screen locked and blank rather than revealing the desktop. That
//! guarantee is only worth anything if this process cannot be brought down by
//! an unrelated bug in the panel or the launcher. So it shares no address space
//! with them, and takes on as few dependencies as it can.

fn main() {
    tracing_subscriber::fmt().init();

    #[cfg(not(target_os = "linux"))]
    {
        tracing::error!("muninn-lock needs a Wayland session; build it on the RavenLinux host");
        std::process::exit(1);
    }

    #[cfg(target_os = "linux")]
    tracing::warn!("not implemented yet");
}
