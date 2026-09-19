//! Which application opens a type: the MIME Applications Associations
//! specification, with one Raven rule for when nobody has chosen.
//!
//! 1. A `[Default Applications]` entry, in any `mimeapps.list`, most
//!    significant file first, for the type or any type it is a kind of.
//! 2. An `[Added Associations]` entry, in the same order, that a more
//!    significant file has not listed under `[Removed Associations]`.
//! 3. Any installed application whose `MimeType=` lists the type -- the
//!    **earliest installed** one. Whoever answers here is written back as the
//!    user's default (see [`Choice::chosen`]), so that installing a second
//!    browser later does not quietly move every link to it.
//!
//! Defaults are tried across the type's whole lineage before associations
//! are, as GLib does: someone who chose an editor for `text/plain` meant it
//! for Python files too, over an IDE that merely claims `text/x-python`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::apps::{App, Apps};

/// The groups of one `mimeapps.list`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MimeApps {
    defaults: HashMap<String, Vec<String>>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, Vec<String>>,
}

impl MimeApps {
    /// Parse a `mimeapps.list`. Unknown groups and malformed lines are
    /// ignored, as the spec requires of a reader.
    pub fn parse(text: &str) -> Self {
        let mut list = Self::default();
        let mut group: Option<&mut HashMap<String, Vec<String>>> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                group = match name {
                    "Default Applications" => Some(&mut list.defaults),
                    "Added Associations" => Some(&mut list.added),
                    "Removed Associations" => Some(&mut list.removed),
                    _ => None,
                };
                continue;
            }
            let (Some(group), Some((mime, ids))) = (group.as_deref_mut(), line.split_once('='))
            else {
                continue;
            };
            let ids = ids
                .split(';')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            group.entry(mime.trim().to_owned()).or_default().extend(ids);
        }
        list
    }

    /// Every list in `paths` that exists, in the same order.
    pub fn load(paths: &[impl AsRef<Path>]) -> Vec<Self> {
        paths
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::parse(&text))
            .collect()
    }
}

/// How a [`Choice`] was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Someone set it: a `[Default Applications]` entry.
    Default,
    /// Listed under `[Added Associations]`.
    Associated,
    /// Nobody chose; this is the earliest-installed application that says it
    /// can open the type.
    Installed,
}

/// The application that opens something, and why it was picked.
#[derive(Debug, Clone, Copy)]
pub struct Choice<'a> {
    /// What will be run.
    pub app: &'a App,
    /// The type it was found for: the type asked about, or one it is a kind
    /// of.
    pub mime: &'a str,
    /// How it was found.
    pub source: Source,
}

impl Choice<'_> {
    /// Whether this was a choice made on nobody's behalf, which should be
    /// recorded as the default so a later install cannot change it.
    pub fn chosen(&self) -> bool {
        self.source != Source::Default
    }
}

/// Choose the application for a type, given its lineage (the type first,
/// then each type it is a kind of) and the `mimeapps.list` files, most
/// significant first.
pub fn choose<'a>(lineage: &'a [String], lists: &[MimeApps], apps: &'a Apps) -> Option<Choice<'a>> {
    for mime in lineage {
        for list in lists {
            for id in list.defaults.get(mime).into_iter().flatten() {
                if let Some(app) = apps.get(id) {
                    return Some(Choice {
                        app,
                        mime,
                        source: Source::Default,
                    });
                }
            }
        }
    }

    for mime in lineage {
        // A removal applies to the file it is in and every less significant
        // one, so it is accumulated on the way down.
        let mut removed: HashSet<&str> = HashSet::new();
        for list in lists {
            for id in list.added.get(mime).into_iter().flatten() {
                if !removed.contains(id.as_str())
                    && let Some(app) = apps.get(id)
                {
                    return Some(Choice {
                        app,
                        mime,
                        source: Source::Associated,
                    });
                }
            }
            removed.extend(
                list.removed
                    .get(mime)
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            );
        }
        if let Some(app) = apps
            .handlers(mime)
            .into_iter()
            .find(|app| !removed.contains(app.id.as_str()))
        {
            return Some(Choice {
                app,
                mime,
                source: Source::Installed,
            });
        }
    }
    None
}

/// Make `id` the default for `mime` in the `mimeapps.list` at `path`,
/// leaving everything else in it as it was. The file is replaced atomically,
/// so a crash mid-write leaves the old one rather than half of a new one.
pub fn set_default(path: &Path, mime: &str, id: &str) -> std::io::Result<()> {
    let old = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let new = with_default(&old, mime, id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("list.raven-open-{}", std::process::id()));
    std::fs::write(&tmp, new)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `text` with `mime=id;` under `[Default Applications]`, replacing any line
/// for `mime` already there and creating the group if there is none.
fn with_default(text: &str, mime: &str, id: &str) -> String {
    let entry = format!("{mime}={id};");
    let mut out: Vec<String> = Vec::new();
    let mut in_defaults = false;
    let mut group_seen = false;
    let mut written = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Leaving the defaults group without having replaced a line:
            // the entry goes at its end, before the next header.
            if in_defaults && !written {
                insert_before_blank_tail(&mut out, &entry);
                written = true;
            }
            in_defaults = trimmed == "[Default Applications]";
            group_seen |= in_defaults;
            out.push(line.to_owned());
            continue;
        }
        if in_defaults
            && let Some((key, _)) = trimmed.split_once('=')
            && key.trim() == mime
        {
            if !written {
                out.push(entry.clone());
                written = true;
            }
            // A duplicate key after the first is dropped: two answers for
            // one type is how a list stops meaning anything.
            continue;
        }
        out.push(line.to_owned());
    }

    if !written {
        if !group_seen {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("[Default Applications]".to_owned());
            out.push(entry);
        } else {
            insert_before_blank_tail(&mut out, &entry);
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Push `line` before any blank lines at the end of `out`, so a group keeps
/// the blank line that separates it from the next.
fn insert_before_blank_tail(out: &mut Vec<String>, line: &str) {
    let at = out
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1);
    out.insert(at, line.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("raven-open-defaults-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app(dir: &Path, id: &str, mimes: &str) {
        std::fs::write(
            dir.join(id),
            format!("[Desktop Entry]\nType=Application\nName={id}\nExec=x %u\nMimeType={mimes}\n"),
        )
        .unwrap();
    }

    fn lineage(types: &[&str]) -> Vec<String> {
        types.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn a_default_wins_over_everything() {
        let dir = tmp("default");
        app(&dir, "a.desktop", "x-scheme-handler/https;");
        app(&dir, "b.desktop", "x-scheme-handler/https;");
        let apps = Apps::scan(std::slice::from_ref(&dir), &[]);
        let lists = [MimeApps::parse(
            "[Added Associations]\nx-scheme-handler/https=a.desktop;\n\
             [Default Applications]\nx-scheme-handler/https=b.desktop;\n",
        )];
        let line = lineage(&["x-scheme-handler/https"]);
        let choice = choose(&line, &lists, &apps).unwrap();
        assert_eq!(
            (choice.app.id.as_str(), choice.source),
            ("b.desktop", Source::Default)
        );
        assert!(!choice.chosen());
    }

    #[test]
    fn a_default_naming_something_uninstalled_is_skipped() {
        let dir = tmp("stale");
        app(&dir, "a.desktop", "text/html;");
        let apps = Apps::scan(std::slice::from_ref(&dir), &[]);
        let lists = [MimeApps::parse(
            "[Default Applications]\ntext/html=gone.desktop;a.desktop;\n",
        )];
        let line = lineage(&["text/html"]);
        assert_eq!(choose(&line, &lists, &apps).unwrap().app.id, "a.desktop");
    }

    #[test]
    fn a_parent_types_default_beats_a_childs_mere_association() {
        let dir = tmp("lineage");
        app(&dir, "ide.desktop", "text/x-python;");
        app(&dir, "editor.desktop", "text/plain;");
        let apps = Apps::scan(std::slice::from_ref(&dir), &[]);
        let lists = [MimeApps::parse(
            "[Default Applications]\ntext/plain=editor.desktop\n",
        )];
        let line = lineage(&["text/x-python", "text/plain"]);
        let choice = choose(&line, &lists, &apps).unwrap();
        assert_eq!(
            (choice.app.id.as_str(), choice.mime),
            ("editor.desktop", "text/plain")
        );
    }

    #[test]
    fn with_no_choice_the_earliest_installed_handler_answers() {
        let dir = tmp("first");
        app(&dir, "zz-first.desktop", "x-scheme-handler/https;");
        std::thread::sleep(std::time::Duration::from_millis(20));
        app(&dir, "aa-second.desktop", "x-scheme-handler/https;");
        let apps = Apps::scan(std::slice::from_ref(&dir), &[]);
        let line = lineage(&["x-scheme-handler/https"]);
        let choice = choose(&line, &[], &apps).unwrap();
        assert_eq!(
            (choice.app.id.as_str(), choice.source),
            ("zz-first.desktop", Source::Installed)
        );
        assert!(choice.chosen());
    }

    #[test]
    fn a_removed_association_is_not_used() {
        let dir = tmp("removed");
        app(&dir, "a.desktop", "image/png;");
        app(&dir, "b.desktop", "image/png;");
        let apps = Apps::scan(std::slice::from_ref(&dir), &[]);
        // The user removed `a`; the system list below still adds it.
        let lists = [
            MimeApps::parse("[Removed Associations]\nimage/png=a.desktop;\n"),
            MimeApps::parse("[Added Associations]\nimage/png=a.desktop;\n"),
        ];
        let line = lineage(&["image/png"]);
        assert_eq!(choose(&line, &lists, &apps).unwrap().app.id, "b.desktop");
    }

    #[test]
    fn nothing_for_the_type_is_none() {
        let apps = Apps::default();
        assert!(choose(&lineage(&["image/png"]), &[], &apps).is_none());
    }

    #[test]
    fn setting_a_default_keeps_the_rest_of_the_file() {
        let before = "# mine\n[Added Associations]\ntext/plain=ed.desktop;\n\n\
                      [Default Applications]\nimage/png=viewer.desktop;\ntext/html=old.desktop;\n\n\
                      [Removed Associations]\nimage/gif=x.desktop;\n";
        let after = with_default(before, "text/html", "brave-browser.desktop");
        assert_eq!(
            after,
            "# mine\n[Added Associations]\ntext/plain=ed.desktop;\n\n\
             [Default Applications]\nimage/png=viewer.desktop;\ntext/html=brave-browser.desktop;\n\n\
             [Removed Associations]\nimage/gif=x.desktop;\n"
        );
    }

    #[test]
    fn setting_a_new_type_appends_it_inside_the_group() {
        let before = "[Default Applications]\nimage/png=viewer.desktop;\n\n[Added Associations]\n";
        assert_eq!(
            with_default(before, "text/html", "b.desktop"),
            "[Default Applications]\nimage/png=viewer.desktop;\ntext/html=b.desktop;\n\n[Added Associations]\n"
        );
    }

    #[test]
    fn setting_into_an_empty_or_groupless_file_creates_the_group() {
        assert_eq!(
            with_default("", "text/html", "b.desktop"),
            "[Default Applications]\ntext/html=b.desktop;\n"
        );
        assert_eq!(
            with_default(
                "[Added Associations]\na/b=c.desktop;\n",
                "text/html",
                "b.desktop"
            ),
            "[Added Associations]\na/b=c.desktop;\n\n[Default Applications]\ntext/html=b.desktop;\n"
        );
    }

    #[test]
    fn set_default_writes_a_file_that_reads_back() {
        let path = tmp("write").join("config/mimeapps.list");
        set_default(&path, "x-scheme-handler/https", "b.desktop").unwrap();
        set_default(&path, "x-scheme-handler/http", "b.desktop").unwrap();
        let list = MimeApps::parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(list.defaults["x-scheme-handler/https"], ["b.desktop"]);
        assert_eq!(list.defaults["x-scheme-handler/http"], ["b.desktop"]);
    }
}
