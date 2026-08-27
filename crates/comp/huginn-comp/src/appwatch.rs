//! Notice applications being installed and removed while the session runs.
//!
//! Without this the application list is a snapshot taken at login: a package
//! installed at 04:07 does not appear in a session that started at 01:54, and
//! the launcher and the dock go on offering what was there when the user last
//! logged in. §4 asks for a freshly installed application to appear without a
//! restart, and this is the watch that delivers it.
//!
//! Watches exactly the directories [`crate::launcher::scan_applications`]
//! reads, by asking [`raven_desktop::directories`] for the same list, so the
//! watch and the scan cannot disagree about where an application may come
//! from. That includes `$XDG_DATA_HOME/applications`, which is the one a user
//! installing something for themselves writes to.
//!
//! Each directory contributes one watch, and which one depends on what exists.
//! The `applications` directory is the interesting one, but it is frequently
//! absent — `/usr/local/share/applications` does not exist until the first
//! `make install` that puts something in it, and `~/.local/share/applications`
//! does not exist until the user installs something for themselves. On a fresh
//! machine the very event most worth catching is the one creating the
//! directory we would otherwise have watched, so when it is missing we watch
//! its parent instead and pick up the real watch on the next rescan.
//!
//! Rescanning is debounced. Installing a package writes a desktop file, an
//! icon, and a dozen unrelated things in a burst, and each of those is its own
//! inotify event; re-reading every desktop file on the system for each one
//! would be a hundred scans where one will do.

use std::cell::Cell;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, LoopHandle, Mode, PostAction};
use inotify::{Inotify, WatchMask};

use crate::xwayland::AsHuginn;

/// How long to let a burst of filesystem events settle before rescanning.
///
/// Long enough that one `make install` is one rescan rather than thirty, short
/// enough that the application is in the launcher before the user has finished
/// reading "Installation complete."
const SETTLE: Duration = Duration::from_millis(250);

/// Big enough for a burst of events in one read; inotify never splits an event
/// across reads, and a short read simply means the next one continues.
const BUFFER: usize = 4096;

/// What counts as a change to an `applications` directory.
///
/// `CLOSE_WRITE` rather than `MODIFY`: an installer writing a desktop file
/// produces several `MODIFY`s and one `CLOSE_WRITE`, and rescanning a file
/// still being written is how you index a half-written entry.
fn entry_mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
}

/// What counts as a change to the parent of an absent `applications`.
///
/// Only the appearance of a subdirectory matters here; everything else that
/// happens in `/usr/share` is none of our business.
fn parent_mask() -> WatchMask {
    WatchMask::CREATE | WatchMask::MOVED_TO
}

/// Add or refresh the watch for every directory, returning how many are watched.
///
/// Safe to call repeatedly: `inotify_add_watch` on a path that is already
/// watched updates that watch in place and returns the same descriptor, so
/// re-arming costs one syscall per directory and never accumulates duplicates.
/// That is what lets a directory which appears later be upgraded from the
/// fallback watch on its parent to the real one.
fn arm(inotify: &Inotify, dirs: &[PathBuf]) -> usize {
    let mut armed = 0;
    for dir in dirs {
        let fallback = dir.parent().map(Path::to_owned);
        if inotify
            .watches()
            .add(dir, entry_mask())
            .or_else(|e| match &fallback {
                Some(parent) => inotify.watches().add(parent, parent_mask()),
                None => Err(e),
            })
            .is_ok()
        {
            armed += 1;
        } else {
            tracing::debug!(
                path = %dir.display(),
                "no watch: neither it nor its parent exists",
            );
        }
    }
    armed
}

/// Watch the application directories and re-index when they change.
///
/// Fail-soft throughout: every failure here costs the session live updates and
/// nothing else, and a compositor that refuses to start because it ran out of
/// inotify watches would be a far worse trade than one that behaves as it did
/// before this existed.
pub(crate) fn start<D>(handle: &LoopHandle<'static, D>)
where
    D: AsHuginn + 'static,
{
    let dirs = raven_desktop::directories();
    let mut inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(e) => {
            tracing::warn!(error = %e, "no inotify: new applications need a restart to appear");
            return;
        }
    };

    let armed = arm(&inotify, &dirs);
    if armed == 0 {
        tracing::warn!(
            "no application directory could be watched; new applications need a restart to appear"
        );
        return;
    }

    // Registered by a duplicate of the descriptor rather than by the `Inotify`
    // itself. calloop hands a source's inner value back to the callback behind
    // `NoIoDrop`, which only derefs immutably without `unsafe`, and reading
    // events needs `&mut`. A dup shares the open file description, so the
    // duplicate signals readable for exactly the queue the original drains.
    let poll_fd = match inotify.as_fd().try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!(error = %e, "could not duplicate the inotify descriptor");
            return;
        }
    };

    // True while a rescan is already scheduled. Without it a burst of events
    // queues a timer per event and rescans thirty times, which is the cost the
    // debounce exists to avoid. Shared because the two halves of the debounce
    // live in different closures: the reader sets it, and only the timer that
    // actually ran the rescan may clear it. `Rc` rather than `Arc` because
    // calloop dispatches both on the loop's own thread.
    let scheduled = Rc::new(Cell::new(false));
    let lh = handle.clone();
    let mut buffer = [0u8; BUFFER];

    let inserted = handle.insert_source(
        Generic::new(poll_fd, Interest::READ, Mode::Level),
        move |_, _, _data: &mut D| {
            // Drain before returning. The source is level-triggered, so a
            // queue left unread wakes the loop again immediately and spins.
            loop {
                match inotify.read_events(&mut buffer) {
                    // The events themselves are not read. Which file changed
                    // does not narrow the work: the scan re-reads every
                    // directory regardless, so the only question an event
                    // answers is whether to run one.
                    Ok(events) => {
                        if events.count() == 0 {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "reading inotify events");
                        break;
                    }
                }
            }

            if scheduled.get() {
                return Ok(PostAction::Continue);
            }

            // Re-arm here rather than after the rescan: this is the point
            // where a directory may have just been created, and doing it once
            // per debounce window rather than once per event keeps it to a
            // handful of syscalls no matter how large the burst.
            arm(&inotify, &dirs);

            scheduled.set(true);
            let done = Rc::clone(&scheduled);
            let timer = Timer::from_duration(SETTLE);
            if let Err(e) = lh.insert_source(timer, move |_, _, data: &mut D| {
                // Cleared before the rescan, not after: a desktop file written
                // while the scan is walking the directory must schedule
                // another pass rather than be swallowed by a flag that is
                // still set.
                done.set(false);
                data.as_huginn().reload_applications();
                TimeoutAction::Drop
            }) {
                tracing::warn!(error = %e, "could not schedule an application rescan");
                scheduled.set(false);
            }
            Ok(PostAction::Continue)
        },
    );

    if let Err(e) = inserted {
        tracing::warn!(error = %e, "inotify source: new applications need a restart to appear");
        return;
    }

    tracing::info!(directories = armed, "watching for installed applications");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway data directory, removed when the test ends.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("raven-appwatch-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("scratch tree");
            Self(root)
        }

        /// A `<base>/applications` path under this tree; `created` says whether
        /// the `applications` directory itself exists yet.
        fn applications(&self, base: &str, created: bool) -> PathBuf {
            let dir = self.0.join(base).join("applications");
            let made = if created { &dir } else { dir.parent().expect("has a parent") };
            std::fs::create_dir_all(made).expect("base dir");
            dir
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_entry(dir: &Path) -> PathBuf {
        let path = dir.join("com.example.App.desktop");
        std::fs::write(&path, "[Desktop Entry]\nType=Application\nName=App\nExec=app\n")
            .expect("desktop file");
        path
    }

    /// Wait for the watch to report something, or give up.
    ///
    /// inotify delivers on the order of microseconds, but "the kernel has
    /// queued it by the time the next line runs" is not a guarantee, and a
    /// test that assumes it fails once a month on a loaded machine. Polling to
    /// a deadline is slower to write and never flakes.
    fn saw_an_event(inotify: &mut Inotify) -> bool {
        let mut buffer = [0u8; BUFFER];
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(events) = inotify.read_events(&mut buffer)
                && events.count() > 0
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn what_is_watched_is_an_applications_directory_with_a_parent_to_fall_back_to() {
        // `arm` falls back to `dir.parent()`, which is only the right thing to
        // watch because the list is `<data dir>/applications` — the parent is
        // then the data directory the install creates it in. A list of bare
        // data directories would silently make the fallback watch the wrong
        // level, so this pins the shape `arm` is written against.
        let dirs = raven_desktop::directories();
        assert!(!dirs.is_empty(), "nothing to watch at all");
        for dir in dirs {
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("applications"));
            assert!(dir.parent().is_some(), "{} has no parent", dir.display());
        }
    }

    #[test]
    fn an_existing_applications_directory_is_watched() {
        let tree = Tree::new("existing");
        let applications = tree.applications("share", true);

        let inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, &[applications]), 1);
    }

    #[test]
    fn a_missing_applications_directory_falls_back_to_its_parent() {
        // The case that produced the bug: /usr/local/share/applications does
        // not exist until the first install puts something there, and
        // ~/.local/share/applications does not exist until the user installs
        // something for themselves. Watching only what exists means missing
        // the install that creates it.
        let tree = Tree::new("missing-applications");
        let applications = tree.applications("share", false);

        let inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, &[applications]), 1);
    }

    #[test]
    fn a_directory_whose_parent_is_absent_too_is_not_watched() {
        let tree = Tree::new("absent");
        let applications = tree.0.join("nothing-here").join("applications");

        let inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, &[applications]), 0);
    }

    #[test]
    fn a_desktop_file_appearing_wakes_the_watch() {
        let tree = Tree::new("install");
        let applications = tree.applications("share", true);

        let mut inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, std::slice::from_ref(&applications)), 1);

        write_entry(&applications);
        assert!(saw_an_event(&mut inotify), "the install went unnoticed");
    }

    #[test]
    fn a_removed_desktop_file_wakes_the_watch() {
        // Uninstall is the same problem in reverse: a dock that goes on
        // offering an application whose binary is gone launches nothing.
        let tree = Tree::new("uninstall");
        let applications = tree.applications("share", true);
        let entry = write_entry(&applications);

        let mut inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, &[applications]), 1);

        std::fs::remove_file(&entry).expect("removing the entry");
        assert!(saw_an_event(&mut inotify), "the removal went unnoticed");
    }

    #[test]
    fn the_fallback_watch_is_upgraded_once_the_directory_appears() {
        // End to end over the exact sequence a first install produces: watch a
        // base with no applications directory, see the directory created,
        // re-arm, and then see the desktop file inside it. Without the re-arm
        // the second half never fires and the application stays invisible
        // until the next login, which is the whole bug.
        let tree = Tree::new("upgrade");
        let applications = tree.applications("share", false);

        let mut inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, std::slice::from_ref(&applications)), 1);

        std::fs::create_dir_all(&applications).expect("applications");
        assert!(
            saw_an_event(&mut inotify),
            "the directory being created went unnoticed"
        );

        arm(&inotify, std::slice::from_ref(&applications));
        write_entry(&applications);
        assert!(
            saw_an_event(&mut inotify),
            "the entry inside the new directory went unnoticed"
        );
    }

    #[test]
    fn a_users_own_application_directory_is_watched_like_any_other() {
        // $XDG_DATA_HOME/applications is where a user installing something for
        // themselves writes, and it was the directory the scan used to ignore
        // entirely. Nothing here is special-cased for it; this asserts that.
        let tree = Tree::new("data-home");
        let system = tree.applications("usr-share", true);
        let home = tree.applications("local-share", true);

        let mut inotify = Inotify::init().expect("inotify");
        assert_eq!(arm(&inotify, &[home.clone(), system]), 2);

        write_entry(&home);
        assert!(
            saw_an_event(&mut inotify),
            "an application installed into the user's own directory went unnoticed"
        );
    }
}
