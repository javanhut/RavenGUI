//! Files the launcher can find by name.
//!
//! An index of what is under the user's home, held in memory and searched
//! the way applications are: type a few characters of the name, get the
//! best matches. Building it walks the tree once; searching it never touches
//! the disk, which is what keeps a keystroke's worth of results inside the
//! frame it was typed in.
//!
//! Everything here is pure — the root is a parameter, the clock is not
//! involved — so the walk and the ranking can be tested against a directory
//! made for the purpose rather than against whatever the machine holds.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::search::{Quality, match_lowercase};

/// One indexed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Where it is.
    pub path: PathBuf,
    /// Its file name, as shown.
    pub name: String,
    /// The name lowercased once, so a search does not lowercase a hundred
    /// thousand names per keystroke.
    lower: String,
    /// Directories below the root. Shallower files rank first among equals:
    /// `~/notes.md` is likelier what was meant than one buried six deep.
    depth: usize,
    /// The directories between the root and the file, each lowercased and
    /// joined by `/`, so a query can name where a file is as well as what
    /// it is called: `raven cargo` finds `~/Development/RavenGUI/Cargo.lock`
    /// and not the four identical `Cargo.lock`s elsewhere. One string
    /// rather than a `Vec<String>` because a hundred thousand of these add
    /// up: a vector of small strings costs an allocation per directory.
    segments: String,
}

/// The lowercased directory names between `root` and `path`, `/`-joined.
fn segments(root: &Path, path: &Path) -> String {
    let Some(parent) = path.parent() else {
        return String::new();
    };
    parent.strip_prefix(root).map_or_else(
        |_| String::new(),
        |rel| {
            rel.components()
                .filter_map(|c| c.as_os_str().to_str())
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
                .join("/")
        },
    )
}

/// How far a walk goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Directories below the root to descend into.
    pub depth: usize,
    /// Files to index before stopping. A bound rather than a guess at
    /// memory: a home with a million files is not one the launcher can
    /// search by name usefully anyway. The walk is breadth-first, so the
    /// bound drops the deepest files, not whichever subtree came last.
    pub files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            depth: 8,
            files: 100_000,
        }
    }
}

/// The fewest characters the last query term needs before files are
/// searched: see [`FileIndex::search`] for why one is too few.
pub const MIN_TERM: usize = 2;

/// Everything found under one root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileIndex {
    root: PathBuf,
    files: Vec<File>,
}

/// Directories not worth walking: build output and dependency trees are
/// enormous, change constantly, and hold nothing a person searches for by
/// name. Dotfiles are skipped for the same reason plus one more — a
/// launcher that surfaces `.ssh/id_ed25519` on typing `id` is worse than
/// one that does not search there.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | "__pycache__" | "venv")
}

impl FileIndex {
    /// Walk `root` and index what it holds, within `limits`.
    ///
    /// Symbolic links are not followed: a link into `/usr` would index the
    /// system, and a link to a parent would never finish. Errors along the
    /// way — an unreadable directory, a vanished entry — skip what they
    /// touch and nothing else.
    ///
    /// The walk is breadth-first: every file at one depth is seen before any
    /// at the next, so when the file cap is hit what is lost is the deepest
    /// — the least likely to be wanted — rather than whole shallow subtrees
    /// chosen by `read_dir` order.
    pub fn build(root: &Path, limits: Limits) -> Self {
        let mut files = Vec::new();
        let mut pending = VecDeque::from([(root.to_owned(), 0usize)]);
        'walk: while let Some((dir, depth)) = pending.pop_front() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if skipped(name) {
                    continue;
                }
                // `file_type` does not follow links, which is the point.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    if depth < limits.depth {
                        pending.push_back((entry.path(), depth + 1));
                    }
                } else if kind.is_file() {
                    let path = entry.path();
                    files.push(File {
                        segments: segments(root, &path),
                        path,
                        name: name.to_owned(),
                        lower: name.to_lowercase(),
                        depth,
                    });
                    if files.len() >= limits.files {
                        break 'walk;
                    }
                }
            }
        }
        // `read_dir` order is whatever the filesystem feels like; two walks
        // of an unchanged tree should compare equal.
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            root: root.to_owned(),
            files,
        }
    }

    /// An index over `paths`, as if they had been found under `root`.
    ///
    /// For callers that already know what they want indexed — tests, and
    /// anything that would rather not walk a disk to get a list.
    pub fn from_paths(root: &Path, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut files: Vec<File> = paths
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_owned();
                let depth = path
                    .strip_prefix(root)
                    .map_or(0, |rel| rel.components().count().saturating_sub(1));
                Some(File {
                    lower: name.to_lowercase(),
                    segments: segments(root, &path),
                    name,
                    path,
                    depth,
                })
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            root: root.to_owned(),
            files,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&File> {
        self.files.get(index)
    }

    /// The directory `index` sits in, relative to the root — `~/Documents`
    /// — for showing beside the name.
    pub fn location(&self, index: usize) -> String {
        let Some(file) = self.files.get(index) else {
            return String::new();
        };
        let Some(parent) = file.path.parent() else {
            return String::new();
        };
        match parent.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => "~".to_owned(),
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => parent.display().to_string(),
        }
    }

    /// The best `limit` matches for `query`, as indices into this index.
    ///
    /// The query is split on whitespace and every term must match: on the
    /// file name, as an application search would, or on some directory
    /// between the root and the file — a prefix, a word start, or merely a
    /// substring of one, since a directory is named by whoever made it and
    /// `raven` has to find `RavenGUI` whatever the casing or the rest.
    ///
    /// Ranked by how well the name matches the last term — the one being
    /// typed, and the one most likely to be the file itself — then by how
    /// shallow the file is, then by name length: among every file that
    /// starts with `not`, `~/notes.md` beats `~/a/b/c/notes-2019-draft-final.md`.
    /// A last term satisfied only by a directory ranks one tier below a
    /// word start, so a file *called* what was typed always beats one merely
    /// *kept* somewhere called that. An empty query matches nothing: with
    /// nothing typed there is nothing meant, and listing the first few
    /// files alphabetically would be noise under the suggestions.
    ///
    /// A last term shorter than [`MIN_TERM`] matches nothing either: a
    /// single letter matches a huge share of a 100k-file index — every
    /// name with an `a` in it — and this runs on the compositor thread on
    /// every keystroke. The user has not said enough to mean a file yet;
    /// the next character will. The earlier terms may be short: they were
    /// typed deliberately and are only narrowing.
    ///
    /// Only the best `limit` hits are kept while scanning — a small sorted
    /// `Vec` that a new hit is inserted into and the tail dropped from — so
    /// the scan costs O(n log limit) and never builds an index-sized `Vec`
    /// to sort. The order is exactly what a full sort would give.
    pub fn search(&self, query: &str, limit: usize) -> Vec<usize> {
        let query = query.to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let Some(last) = terms.last() else {
            return Vec::new();
        };
        if limit == 0 || last.chars().count() < MIN_TERM {
            return Vec::new();
        }
        // Best first; `limit` long at most.
        let mut best: Vec<(Quality, usize, usize, usize)> = Vec::with_capacity(limit + 1);
        for (i, f) in self.files.iter().enumerate() {
            let mut quality = None;
            for term in &terms {
                let Some(q) = f.matches(term) else {
                    quality = None;
                    break;
                };
                quality = Some(q);
            }
            let Some(quality) = quality else { continue };
            let hit = (quality, f.depth, f.name.len(), i);
            let rank = |a: &(Quality, usize, usize, usize), b: &(Quality, usize, usize, usize)| {
                b.0.cmp(&a.0)
                    .then(a.1.cmp(&b.1))
                    .then(a.2.cmp(&b.2))
                    .then_with(|| self.files[a.3].name.cmp(&self.files[b.3].name))
            };
            // A hit that ranks no better than the current worst of a full
            // list can be skipped without the binary search: this is the
            // common case for a broad term, and keeps the scan cheap.
            if best.len() == limit && rank(&hit, best.last().unwrap()) != Ordering::Less {
                continue;
            }
            // `partition_point` gives the first slot that ranks worse, so
            // equal-ranking hits keep index order, as a stable sort would.
            let at = best.partition_point(|b| rank(b, &hit) != Ordering::Greater);
            best.insert(at, hit);
            best.truncate(limit);
        }
        best.into_iter().map(|h| h.3).collect()
    }
}

impl File {
    /// How well one lowercased `term` matches this file, if at all: by name
    /// at the name's own quality, else by a directory on the way to it at
    /// the tier below a word start.
    fn matches(&self, term: &str) -> Option<Quality> {
        match_lowercase(&self.lower, term).or_else(|| {
            self.segments
                .split('/')
                .any(|s| s.contains(term))
                .then_some(Quality::Subsequence)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::from_paths(
            Path::new("/home/u"),
            paths.iter().map(|p| PathBuf::from(format!("/home/u/{p}"))),
        )
    }

    fn names(index: &FileIndex, hits: &[usize]) -> Vec<String> {
        hits.iter()
            .map(|i| index.get(*i).unwrap().name.clone())
            .collect()
    }

    #[test]
    fn nothing_typed_finds_nothing() {
        let idx = index(&["notes.md", "a.txt"]);
        assert!(idx.search("", 5).is_empty());
        assert!(idx.search("   ", 5).is_empty());
    }

    #[test]
    fn a_prefix_beats_a_word_start_beats_a_scattered_match() {
        let idx = index(&["report-notes.md", "notes.md", "n_o_t_e_s.md"]);
        assert_eq!(
            names(&idx, &idx.search("not", 5)),
            ["notes.md", "report-notes.md", "n_o_t_e_s.md"]
        );
    }

    #[test]
    fn among_equals_the_shallower_and_shorter_wins() {
        let idx = index(&["deep/er/still/notes.md", "notes.md", "notes-2019-draft.md"]);
        assert_eq!(
            names(&idx, &idx.search("notes", 5)),
            ["notes.md", "notes-2019-draft.md", "notes.md"]
        );
        let hits = idx.search("notes", 5);
        assert_eq!(idx.location(hits[0]), "~");
        assert_eq!(idx.location(hits[2]), "~/deep/er/still");
    }

    #[test]
    fn a_one_character_last_term_finds_no_files() {
        let idx = index(&["notes.md", "n.md", "docs/n"]);
        assert!(idx.search("n", 5).is_empty());
        assert!(idx.search("notes n", 5).is_empty());
        assert!(idx.search("  n  ", 5).is_empty());
        // Two characters is enough, and an earlier one-character term
        // merely narrows.
        assert_eq!(names(&idx, &idx.search("n.", 5)), ["n.md", "notes.md"]);
        assert_eq!(names(&idx, &idx.search("n notes", 5)), ["notes.md"]);
        // Counted in characters, not bytes: `é` is one.
        assert!(idx.search("\u{e9}", 5).is_empty());
    }

    #[test]
    fn the_bounded_selection_orders_like_a_full_sort() {
        let paths = [
            "notes.md",
            "a/notes.md",
            "a/b/notes.md",
            "notes-2019-draft.md",
            "my-notes.md",
            "docs/notes.txt",
            "notes/plan.md",
            "notes/z.md",
            "n_o_t_e_s.md",
            "b/notes.md",
            "report-notes.md",
            "a/notes.txt",
        ];
        let idx = index(&paths);
        // The full list, in rank order, is what every prefix must equal.
        let all = idx.search("notes", paths.len());
        assert_eq!(all.len(), paths.len());
        for limit in 1..=paths.len() {
            assert_eq!(idx.search("notes", limit), all[..limit], "limit {limit}");
        }
        assert_eq!(idx.search("notes", 100), all);
        // The shape of the order itself, so the expectation is not merely
        // self-consistent: by quality, then depth, then length, then name.
        assert_eq!(
            names(&idx, &all[..4]),
            ["notes.md", "notes-2019-draft.md", "notes.md", "notes.md"]
        );
        assert_eq!(idx.location(all[0]), "~");
        assert_eq!(idx.location(all[1]), "~");
        assert_eq!(idx.location(all[2]), "~/a");
        assert_eq!(idx.location(all[3]), "~/b");
    }

    #[test]
    fn matching_ignores_case_and_the_limit_holds() {
        let idx = index(&["Notes.md", "NOTES.txt", "notes.org"]);
        assert_eq!(idx.search("notes", 2).len(), 2);
        assert_eq!(idx.search("NoTeS", 5).len(), 3);
    }

    #[test]
    fn a_directory_in_the_query_tells_identical_names_apart() {
        let idx = index(&[
            "Development/Ivaldi/Cargo.lock",
            "Development/RavenGUI/Cargo.lock",
            "Development/RavenGUI/README.md",
        ]);
        let hits = idx.search("raven cargo", 5);
        assert_eq!(names(&idx, &hits), ["Cargo.lock"]);
        assert_eq!(idx.location(hits[0]), "~/Development/RavenGUI");
        // The other way round works too: the name need not come last.
        let hits = idx.search("cargo raven", 5);
        assert_eq!(idx.location(hits[0]), "~/Development/RavenGUI");
    }

    #[test]
    fn a_term_that_matches_nothing_sinks_the_whole_query() {
        let idx = index(&["Development/RavenGUI/Cargo.lock"]);
        assert!(idx.search("raven cargo zebra", 5).is_empty());
        assert!(idx.search("zebra", 5).is_empty());
    }

    #[test]
    fn a_name_that_matches_beats_a_directory_that_does() {
        let idx = index(&["notes/plan.md", "docs/notes.md", "a/b/notes.txt"]);
        assert_eq!(
            names(&idx, &idx.search("notes", 5)),
            ["notes.md", "notes.txt", "plan.md"]
        );
        // The file name itself is never a "segment": `plan` by directory
        // would be nonsense.
        assert!(idx.search("plan.md/x", 5).is_empty());
    }

    #[test]
    fn walking_records_the_directories_on_the_way() {
        let dir = std::env::temp_dir().join(format!("raven-segs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Docs/Sub")).unwrap();
        std::fs::write(dir.join("Docs/Sub/plan.md"), "").unwrap();
        let idx = FileIndex::build(&dir, Limits::default());
        assert_eq!(idx.files[0].segments, "docs/sub");
        assert_eq!(idx.search("sub plan", 5).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walking_skips_dotfiles_dependency_trees_and_links() {
        let dir = std::env::temp_dir().join(format!("raven-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs/sub")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".ssh")).unwrap();
        std::fs::write(dir.join("notes.md"), "").unwrap();
        std::fs::write(dir.join("docs/sub/plan.md"), "").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::write(dir.join(".ssh/id"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", dir.join("root")).unwrap();

        let idx = FileIndex::build(&dir, Limits::default());
        let mut found: Vec<String> = idx.files.iter().map(|f| f.name.clone()).collect();
        found.sort();
        assert_eq!(found, ["notes.md", "plan.md"]);

        // Depth and count limits are honoured.
        let shallow = FileIndex::build(
            &dir,
            Limits {
                depth: 0,
                files: 100,
            },
        );
        assert_eq!(shallow.len(), 1, "depth 0 should see only the root's files");
        let one = FileIndex::build(&dir, Limits { depth: 8, files: 1 });
        assert_eq!(one.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capped_walk_keeps_the_shallowest_files() {
        let dir = std::env::temp_dir().join(format!("raven-bfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Directory names sort before the root files, so a depth-first walk
        // driven by `read_dir` order could well fill the cap from `a/`.
        for d in ["a/x", "b/y"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            "a/1.txt",
            "a/2.txt",
            "a/x/3.txt",
            "b/4.txt",
            "b/y/5.txt",
            "z1.txt",
            "z2.txt",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let idx = FileIndex::build(&dir, Limits { depth: 8, files: 5 });
        assert_eq!(idx.len(), 5);
        let depths: Vec<usize> = idx.files.iter().map(|f| f.depth).collect();
        assert!(
            depths.iter().filter(|&&d| d == 0).count() == 2,
            "{depths:?}"
        );
        assert!(depths.iter().all(|&d| d < 2), "{depths:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
