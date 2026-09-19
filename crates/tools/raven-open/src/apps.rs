//! What is installed, read straight from the `.desktop` files.
//!
//! Deliberately not from `mimeinfo.cache`: that file is only as fresh as the
//! last run of the tool that writes it, and a browser installed a moment ago
//! must open the next link. Scanning a few hundred small files is quick
//! enough to do on every open.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use raven_desktop::Entry;

/// How deep application subdirectories are followed. Real trees use one
/// level (`kde4/`); the bound is so a symlink loop cannot hang an open.
const MAX_DEPTH: usize = 4;

/// One installed application that can be run.
#[derive(Debug, Clone)]
pub struct App {
    /// Its desktop file ID: the path under `applications/` with `/` turned
    /// into `-`, e.g. `brave-browser.desktop`.
    pub id: String,
    /// The parsed entry.
    pub entry: Entry,
    /// When its `.desktop` file was written to this disk, as
    /// `(seconds, nanoseconds)` of the inode change time. Unlike the
    /// modification time, a package cannot set this to its build date, so it
    /// orders applications by when they were actually installed.
    pub installed: (i64, i64),
}

/// Every installed application, by desktop file ID.
#[derive(Debug, Default)]
pub struct Apps {
    by_id: HashMap<String, App>,
}

impl Apps {
    /// Scan `app_dirs`, most significant first.
    ///
    /// The first file for an ID wins, even when it cannot be used: a user's
    /// `~/.local/share/applications/foo.desktop` with `Hidden=true` is how
    /// the spec says to delete the system's `foo.desktop`, so it must mask
    /// it rather than fall through to it.
    pub fn scan(app_dirs: &[PathBuf], desktops: &[String]) -> Self {
        let mut apps = Self::default();
        let mut seen = HashSet::new();
        for dir in app_dirs {
            let mut files = Vec::new();
            walk(dir, dir, 0, &mut files);
            // Sorted so the result does not depend on directory order.
            files.sort();
            for (id, path) in files {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(entry) = raven_desktop::parse_handler(&text, &path, desktops) else {
                    continue;
                };
                let installed = std::fs::metadata(&path)
                    .map(|m| (m.ctime(), m.ctime_nsec()))
                    .unwrap_or((i64::MAX, 0));
                apps.by_id.insert(
                    id.clone(),
                    App {
                        id,
                        entry,
                        installed,
                    },
                );
            }
        }
        apps
    }

    /// The application with this desktop file ID, if it is installed and
    /// usable.
    pub fn get(&self, id: &str) -> Option<&App> {
        self.by_id.get(id)
    }

    /// Every application whose `MimeType=` lists `mime`, earliest installed
    /// first -- the order that makes the first browser installed the one
    /// that stays default.
    pub fn handlers(&self, mime: &str) -> Vec<&App> {
        let mut apps: Vec<&App> = self
            .by_id
            .values()
            .filter(|app| app.entry.mime_types.iter().any(|t| t == mime))
            .collect();
        apps.sort_by(|a, b| a.installed.cmp(&b.installed).then_with(|| a.id.cmp(&b.id)));
        apps
    }
}

/// Collect `(id, path)` for every `.desktop` file under `dir`.
fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        let path = item.path();
        // The entry's own type, not the target's: a symlinked directory is
        // not descended into, which is what keeps a loop from recursing.
        let Ok(kind) = item.file_type() else { continue };
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                walk(root, &path, depth + 1, out);
            }
        } else if path.extension().is_some_and(|e| e == "desktop")
            && let Ok(rel) = path.strip_prefix(root)
            && let Some(rel) = rel.to_str()
        {
            out.push((rel.replace('/', "-"), path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_app(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("[Desktop Entry]\nType=Application\n{body}")).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("raven-open-apps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn subdirectories_become_part_of_the_id() {
        let dir = tmp("ids");
        write_app(
            &dir,
            "kde4/viewer.desktop",
            "Name=V\nExec=v %f\nMimeType=image/png;\n",
        );
        let apps = Apps::scan(&[dir], &[]);
        assert!(apps.get("kde4-viewer.desktop").is_some());
    }

    #[test]
    fn a_users_hidden_copy_deletes_the_system_entry() {
        let (user, system) = (tmp("mask-user"), tmp("mask-system"));
        write_app(
            &system,
            "b.desktop",
            "Name=B\nExec=b %u\nMimeType=x-scheme-handler/https;\n",
        );
        write_app(&user, "b.desktop", "Name=B\nExec=b %u\nHidden=true\n");
        let apps = Apps::scan(&[user, system], &[]);
        assert!(apps.get("b.desktop").is_none());
        assert!(apps.handlers("x-scheme-handler/https").is_empty());
    }

    #[test]
    fn a_users_copy_replaces_the_system_entry() {
        let (user, system) = (tmp("over-user"), tmp("over-system"));
        write_app(
            &system,
            "e.desktop",
            "Name=System\nExec=e %f\nMimeType=text/plain;\n",
        );
        write_app(
            &user,
            "e.desktop",
            "Name=Mine\nExec=e --mine %f\nMimeType=text/plain;\n",
        );
        let apps = Apps::scan(&[user, system], &[]);
        assert_eq!(apps.get("e.desktop").unwrap().entry.name, "Mine");
    }

    #[test]
    fn handlers_hidden_from_menus_are_found() {
        let dir = tmp("nodisplay");
        write_app(
            &dir,
            "h.desktop",
            "Name=H\nExec=h %u\nNoDisplay=true\nMimeType=x-scheme-handler/magnet;\n",
        );
        let apps = Apps::scan(&[dir], &[]);
        assert_eq!(apps.handlers("x-scheme-handler/magnet").len(), 1);
    }

    #[test]
    fn handlers_come_earliest_installed_first() {
        let dir = tmp("order");
        write_app(
            &dir,
            "zfirst.desktop",
            "Name=Z\nExec=z %u\nMimeType=x-scheme-handler/https;\n",
        );
        // ctime has nanosecond resolution on every filesystem Raven installs
        // to, but a coarse one would tie; the id breaks ties, so the later
        // file is named to lose that too if it comes to it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_app(
            &dir,
            "zzsecond.desktop",
            "Name=Y\nExec=y %u\nMimeType=x-scheme-handler/https;\n",
        );
        let apps = Apps::scan(&[dir], &[]);
        let ids: Vec<_> = apps
            .handlers("x-scheme-handler/https")
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids, ["zfirst.desktop", "zzsecond.desktop"]);
    }
}
