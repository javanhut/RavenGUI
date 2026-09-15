//! Notice `desktop.toml`, and the wallpaper it points at, changing while the
//! session runs.
//!
//! The settings application writes the file atomically — a sibling temp file
//! renamed into place — so the event that matters is `MOVED_TO` on the
//! directory, with `CLOSE_WRITE` for anyone editing it by hand. Same shape as
//! [`crate::appwatch`], and fail-soft for the same reason: losing live reload
//! costs a logout, and a compositor that will not start over an inotify
//! failure costs the machine.
//!
//! The wallpaper is watched here too, rather than only re-read when the path
//! in `desktop.toml` changes, because the path usually does not: the settings
//! application writes every pick to the same `wallpaper.<ext>`, and
//! `set-wallpaper.sh` repoints `set/` without touching any config at all. The
//! directories come from [`crate::wallpaper::directories`] and are re-armed
//! after every reload, since a new path in the file means new ones.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, LoopHandle, Mode, PostAction};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::xwayland::AsHuginn;

/// Let a save settle before re-reading: one write is one reload.
const SETTLE: Duration = Duration::from_millis(200);

const BUFFER: usize = 4096;

/// Whether an event says the file it names is ready to read.
///
/// A `CREATE` of a plain file is the start of a copy, not the end: `cp` and
/// `install` create the file and then write it, and a large picture can take
/// longer than [`SETTLE`] to arrive, so reading on the create decodes half of
/// one. Its `CLOSE_WRITE` follows and is the event that counts. A symlink or a
/// hard link is complete the moment it exists and never gets a `CLOSE_WRITE`,
/// so those count on the create, as does a directory.
fn settled(mask: EventMask, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if !mask.contains(EventMask::CREATE) || mask.contains(EventMask::ISDIR) {
        return true;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_symlink() || meta.nlink() > 1,
        // Gone already; whatever moved or removed it sends its own event.
        Err(_) => false,
    }
}

/// One mask for every watch. inotify keeps one watch per directory, and adding
/// a directory that is already watched *replaces* its mask — so a wallpaper
/// kept in `~/.config/raven` must not narrow the config file's watch, or the
/// other way round.
fn mask() -> WatchMask {
    WatchMask::CLOSE_WRITE
        | WatchMask::MOVED_TO
        | WatchMask::MOVED_FROM
        | WatchMask::CREATE
        | WatchMask::DELETE
}

struct Watches {
    inotify: Inotify,
    config_dir: PathBuf,
    /// The config directory's watch, or its parent's while it does not exist.
    config: HashMap<WatchDescriptor, PathBuf>,
    wallpaper: HashMap<WatchDescriptor, PathBuf>,
}

impl Watches {
    /// The directory, not the file: a rename replaces the inode, and a watch
    /// on the old one would go quiet after the first save. When even the
    /// directory is missing, watch its parent for it to appear; the settings
    /// app creates it on first save.
    fn arm_config(&mut self) {
        let mut watches = self.inotify.watches();
        let armed = match watches.add(&self.config_dir, mask()) {
            Ok(wd) => Some((wd, self.config_dir.clone())),
            Err(_) => self
                .config_dir
                .parent()
                .and_then(|p| watches.add(p, mask()).ok().map(|wd| (wd, p.to_owned()))),
        };
        if let Some((wd, dir)) = armed {
            self.config.insert(wd, dir);
        }
    }

    /// Watch exactly `directories` for the wallpaper, dropping the ones no
    /// longer wanted — but never one the config file shares.
    fn arm_wallpaper(&mut self, directories: &[PathBuf]) {
        let mut watches = self.inotify.watches();
        let mut next = HashMap::new();
        for dir in directories {
            match watches.add(dir, mask()) {
                Ok(wd) => {
                    next.insert(wd, dir.clone());
                }
                Err(e) => tracing::debug!(path = %dir.display(), "not watching for the wallpaper: {e}"),
            }
        }
        for (wd, _) in self.wallpaper.drain() {
            if !next.contains_key(&wd) && !self.config.contains_key(&wd) {
                // Fails for a directory that has since gone, which removed
                // the watch already.
                let _ = watches.remove(wd);
            }
        }
        self.wallpaper = next;
    }
}

pub(crate) fn start<D>(handle: &LoopHandle<'static, D>)
where
    D: AsHuginn + 'static,
{
    let Some(path) = crate::desktop_config::path() else {
        return;
    };
    let Some(config_dir) = path.parent().map(Path::to_owned) else {
        return;
    };
    let inotify = match Inotify::init() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "no inotify: settings and wallpaper changes need a relogin");
            return;
        }
    };
    let poll_fd = match inotify.as_fd().try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!(error = %e, "could not duplicate the inotify descriptor");
            return;
        }
    };

    let mut watches = Watches {
        inotify,
        config_dir,
        config: HashMap::new(),
        wallpaper: HashMap::new(),
    };
    watches.arm_config();
    // Read here rather than taken from the compositor's state, which this has
    // no handle on yet; the first reload re-arms from the state itself.
    let chosen = crate::desktop_config::DesktopConfig::load().wallpaper();
    watches.arm_wallpaper(&crate::wallpaper::directories(chosen.as_deref()));
    if watches.config.is_empty() && watches.wallpaper.is_empty() {
        tracing::debug!("no config or wallpaper directory to watch");
        return;
    }
    let watches = Rc::new(RefCell::new(watches));

    let scheduled = Rc::new(Cell::new(false));
    // Whether any event since the last reload was the config file itself, as
    // opposed to only something near a wallpaper.
    let config_changed = Rc::new(Cell::new(false));
    let lh = handle.clone();
    let mut buffer = [0u8; BUFFER];
    let file_name: Option<OsString> = path.file_name().map(|n| n.to_owned());

    let reader_watches = Rc::clone(&watches);
    let inserted = handle.insert_source(
        Generic::new(poll_fd, Interest::READ, Mode::Level),
        move |_, _, _data: &mut D| {
            let mut relevant = false;
            let mut w = reader_watches.borrow_mut();
            loop {
                match w.inotify.read_events(&mut buffer) {
                    Ok(events) => {
                        let mut any = false;
                        let mut rearm_config = false;
                        for event in events {
                            any = true;
                            let Some(dir) = w
                                .config
                                .get(&event.wd)
                                .or_else(|| w.wallpaper.get(&event.wd))
                            else {
                                continue;
                            };
                            if let Some(name) = event.name
                                && !settled(event.mask, &dir.join(name))
                            {
                                continue;
                            }
                            if w.config.contains_key(&event.wd) {
                                // Either the file itself, or the `raven`
                                // directory appearing under ~/.config (then
                                // re-arm on it).
                                match event.name {
                                    Some(name) if Some(name) == file_name.as_deref() => {
                                        config_changed.set(true);
                                        relevant = true;
                                    }
                                    Some(name) if name == "raven" => {
                                        rearm_config = true;
                                        config_changed.set(true);
                                        relevant = true;
                                    }
                                    _ => {}
                                }
                            }
                            // Any change at all: which file it was does not
                            // narrow the work, since the refresh compares the
                            // wallpaper's stamps and is a stat when unchanged.
                            if w.wallpaper.contains_key(&event.wd) {
                                relevant = true;
                            }
                        }
                        if rearm_config {
                            w.arm_config();
                        }
                        if !any {
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
            drop(w);
            if !relevant || scheduled.get() {
                return Ok(PostAction::Continue);
            }
            scheduled.set(true);
            let done = Rc::clone(&scheduled);
            let config_changed = Rc::clone(&config_changed);
            let timer_watches = Rc::clone(&reader_watches);
            if let Err(e) =
                lh.insert_source(Timer::from_duration(SETTLE), move |_, _, data: &mut D| {
                    done.set(false);
                    let huginn = data.as_huginn();
                    if config_changed.replace(false) {
                        // Refreshes the wallpaper too.
                        huginn.reload_desktop_config();
                    } else {
                        huginn.refresh_wallpaper();
                    }
                    let directories = huginn.wallpaper_directories();
                    timer_watches.borrow_mut().arm_wallpaper(&directories);
                    TimeoutAction::Drop
                })
            {
                tracing::warn!(error = %e, "could not schedule a settings reload");
                scheduled.set(false);
            }
            Ok(PostAction::Continue)
        },
    );
    if let Err(e) = inserted {
        tracing::warn!(error = %e, "inotify source: settings and wallpaper changes need a relogin");
        return;
    }
    tracing::info!(path = %path.display(), "watching the desktop settings file and the wallpaper");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("huginn-configwatch-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("scratch dir");
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The gap this closes: `cp` onto a watched directory creates the file
    /// empty and fills it afterwards.
    #[test]
    fn a_plain_file_being_created_waits_for_its_close() {
        let scratch = Scratch::new("create");
        let file = scratch.0.join("wallpaper.jpg");
        let _open = std::fs::File::create(&file).expect("create");
        assert!(!settled(EventMask::CREATE, &file));
        assert!(settled(EventMask::CLOSE_WRITE, &file));
        assert!(settled(EventMask::MOVED_TO, &file));
    }

    #[test]
    fn links_and_directories_are_complete_when_created() {
        let scratch = Scratch::new("links");
        let target = scratch.0.join("cliff.jpg");
        std::fs::write(&target, b"x").expect("write");

        let symlink = scratch.0.join("wallpaper.jpg");
        std::os::unix::fs::symlink(&target, &symlink).expect("symlink");
        assert!(settled(EventMask::CREATE, &symlink));

        let hard = scratch.0.join("wallpaper.png");
        std::fs::hard_link(&target, &hard).expect("hard link");
        assert!(settled(EventMask::CREATE, &hard));

        let dir = scratch.0.join("raven");
        std::fs::create_dir(&dir).expect("mkdir");
        assert!(settled(EventMask::CREATE | EventMask::ISDIR, &dir));
    }

    #[test]
    fn a_file_gone_before_its_create_is_read_is_not_ready() {
        let scratch = Scratch::new("gone");
        assert!(!settled(EventMask::CREATE, &scratch.0.join("nothing")));
    }
}
