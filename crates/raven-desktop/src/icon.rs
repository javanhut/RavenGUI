//! Turning `Icon=raven-terminal` into a file on disk.
//!
//! The XDG Icon Theme lookup, which is more involved than it sounds: a theme
//! inherits from other themes, each theme's `index.theme` declares directories
//! with their own sizes and scaling rules, and the answer for a given size is
//! whichever directory matches it best rather than the first one found.
//!
//! The case this exists to get right, from the installed RavenTerminal:
//!
//! ```text
//! Icon=raven-terminal
//!   -> /usr/share/icons/hicolor/scalable/apps/raven-terminal.svg
//! ```
//!
//! There is no PNG at any size, so a lookup that only understands raster
//! directories finds nothing at all. Scalable directories are not an
//! optimisation here; they are the only thing that works.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Extensions to try, best first.
///
/// SVG outranks PNG because it is exact at any size, and `.svgz` is a real
/// thing in shipped themes rather than a curiosity. XPM is last and legacy.
const EXTENSIONS: &[&str] = &["svg", "svgz", "png", "xpm"];

/// The theme every theme falls back to, and where unthemed icons live.
const FALLBACK_THEME: &str = "hicolor";

/// How a directory's icons scale, from its `Type` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeKind {
    /// Exactly `size`.
    Fixed,
    /// Anything from `min` to `max`, rendered to order.
    Scalable,
    /// Within `threshold` of `size`, and stretched the rest of the way.
    Threshold,
}

/// One directory listed in an `index.theme`.
#[derive(Debug, Clone)]
struct ThemeDir {
    path: String,
    size: u32,
    scale: u32,
    kind: SizeKind,
    min: u32,
    max: u32,
    threshold: u32,
}

impl ThemeDir {
    /// Whether this directory can serve `size` at `scale` exactly.
    fn matches(&self, size: u32, scale: u32) -> bool {
        if self.scale != scale {
            return false;
        }
        match self.kind {
            SizeKind::Fixed => self.size == size,
            // Inclusive on both ends. An off-by-one here silently rejects the
            // scalable directory at its declared MinSize or MaxSize, which for
            // an SVG-only theme means finding no icon at all.
            SizeKind::Scalable => self.min <= size && size <= self.max,
            SizeKind::Threshold => {
                self.size.abs_diff(size) <= self.threshold
            }
        }
    }

    /// How far this directory is from serving `size`, for ranking near misses.
    ///
    /// Only consulted when nothing matches exactly, so that a 48px request
    /// against a theme with 32 and 64 gets the 64 rather than nothing.
    fn distance(&self, size: u32, scale: u32) -> u32 {
        let scaled = |v: u32| v * self.scale;
        let want = size * scale;
        match self.kind {
            SizeKind::Fixed => scaled(self.size).abs_diff(want),
            SizeKind::Scalable => {
                if want < scaled(self.min) {
                    scaled(self.min) - want
                } else if want > scaled(self.max) {
                    want - scaled(self.max)
                } else {
                    0
                }
            }
            SizeKind::Threshold => {
                let lo = scaled(self.size.saturating_sub(self.threshold));
                let hi = scaled(self.size + self.threshold);
                // Zero anywhere inside the band; the distance outside it
                // otherwise. Saturating on both sides so neither subtraction
                // can wrap on the far side of the band.
                lo.saturating_sub(want).max(want.saturating_sub(hi))
            }
        }
    }
}

/// A parsed `index.theme`.
#[derive(Debug, Clone)]
struct Theme {
    dirs: Vec<ThemeDir>,
    inherits: Vec<String>,
}

/// Icon themes, indexed once so lookups are cheap.
///
/// Building this walks every theme directory and parses every `index.theme`,
/// which costs on the order of a hundred milliseconds on a machine with many
/// themes installed. That is fine once at startup and far too slow per
/// keystroke, which is the entire reason this is a struct you keep rather than
/// a function you call.
#[derive(Debug)]
pub struct Icons {
    /// Theme name to its parsed index, for every theme found in every base
    /// directory. A theme split across `/usr/share` and `~/.local/share` is
    /// merged here, which is how a user overrides one icon of a system theme.
    themes: HashMap<String, Theme>,
    /// Base directories, in search precedence.
    bases: Vec<PathBuf>,
    /// The theme to consult first.
    theme: String,
}

impl Icons {
    /// Index the icon themes installed for this user.
    ///
    /// `theme` is the preferred theme name; `hicolor` is always consulted as
    /// the final fallback whether or not it is named.
    pub fn discover(theme: &str) -> Self {
        Self::with_bases(theme, default_bases())
    }

    /// [`Self::discover`] against explicit base directories, for tests.
    pub fn with_bases(theme: &str, bases: Vec<PathBuf>) -> Self {
        let mut themes: HashMap<String, Theme> = HashMap::new();

        for base in &bases {
            let Ok(entries) = std::fs::read_dir(base) else {
                continue;
            };
            for entry in entries.flatten() {
                let dir = entry.path();
                let Ok(text) = std::fs::read_to_string(dir.join("index.theme")) else {
                    continue;
                };
                let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let parsed = parse_index(&text);
                // A theme present in two base directories is one theme with
                // the union of its directories, not two competing ones.
                match themes.get_mut(name) {
                    Some(existing) => {
                        existing.dirs.extend(parsed.dirs);
                        existing.inherits.extend(parsed.inherits);
                    }
                    None => {
                        themes.insert(name.to_owned(), parsed);
                    }
                }
            }
        }

        Self {
            themes,
            bases,
            theme: theme.to_owned(),
        }
    }

    /// Find the best file for `name` at `size` and `scale`.
    ///
    /// An `Icon=` that is already an absolute path is returned as-is when it
    /// exists — the spec allows it, and several real entries use it.
    pub fn find(&self, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
        if name.starts_with('/') {
            let path = PathBuf::from(name);
            return path.is_file().then_some(path);
        }
        // An Icon= value is occasionally written with its extension despite
        // the spec saying not to. Strip it rather than search for
        // "foo.png.svg".
        let name = strip_known_extension(name);

        for theme in self.search_order() {
            if let Some(found) = self.find_in_theme(&theme, name, size, scale) {
                return Some(found);
            }
        }
        // Last resort: loose icons sitting directly in a base directory, which
        // is where things land that belong to no theme at all.
        self.bases
            .iter()
            .find_map(|base| first_existing(base, name))
    }

    /// The preferred theme, everything it inherits, then hicolor.
    ///
    /// Breadth-first with a seen-set: theme inheritance is a graph and real
    /// themes do contain cycles, which a naive walk follows forever.
    fn search_order(&self) -> Vec<String> {
        let mut order = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::from([self.theme.clone()]);

        while let Some(theme) = queue.pop_front() {
            if !seen.insert(theme.clone()) {
                continue;
            }
            if let Some(t) = self.themes.get(&theme) {
                queue.extend(t.inherits.iter().cloned());
            }
            order.push(theme);
        }
        if !seen.contains(FALLBACK_THEME) {
            order.push(FALLBACK_THEME.to_owned());
        }
        order
    }

    /// Best file for `name` within one theme, or `None`.
    fn find_in_theme(&self, theme: &str, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
        let dirs = &self.themes.get(theme)?.dirs;

        // An exact match wins outright, whatever else is available.
        for dir in dirs.iter().filter(|d| d.matches(size, scale)) {
            for base in &self.bases {
                if let Some(found) = first_existing(&base.join(theme).join(&dir.path), name) {
                    return Some(found);
                }
            }
        }

        // Otherwise the closest, so a request no directory serves exactly
        // still produces an icon rather than a blank space.
        let mut candidates: Vec<_> = dirs
            .iter()
            .filter_map(|dir| {
                self.bases.iter().find_map(|base| {
                    first_existing(&base.join(theme).join(&dir.path), name)
                        .map(|path| (dir.distance(size, scale), path))
                })
            })
            .collect();
        candidates.sort_by_key(|(distance, _)| *distance);
        candidates.into_iter().next().map(|(_, path)| path)
    }
}

/// The first of `dir/name.{svg,svgz,png,xpm}` that exists.
fn first_existing(dir: &Path, name: &str) -> Option<PathBuf> {
    EXTENSIONS.iter().find_map(|ext| {
        let path = dir.join(format!("{name}.{ext}"));
        path.is_file().then_some(path)
    })
}

/// Drop a trailing `.png`/`.svg`/... that should not have been in `Icon=`.
fn strip_known_extension(name: &str) -> &str {
    name.rsplit_once('.')
        .filter(|(_, ext)| EXTENSIONS.contains(ext))
        .map_or(name, |(stem, _)| stem)
}

/// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, as icon base directories.
///
/// User directories come first so a user icon overrides the system's — which
/// is the whole reason the precedence is specified at all.
fn default_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        // Predates the XDG basedir spec and is still consulted first.
        bases.push(PathBuf::from(&home).join(".icons"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(dir) = data_home {
        bases.push(dir.join("icons"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    bases.extend(std::env::split_paths(&data_dirs).map(|d| d.join("icons")));
    // Not an XDG directory, but decades of software install here anyway.
    bases.push(PathBuf::from("/usr/share/pixmaps"));
    bases
}

/// Parse an `index.theme`.
fn parse_index(text: &str) -> Theme {
    let mut dirs = Vec::new();
    let mut inherits = Vec::new();
    let mut group: Option<String> = None;
    let mut fields: HashMap<String, String> = HashMap::new();

    // Directory groups end at the next `[`, so each is flushed when the
    // following group starts, and the last one when the text runs out.
    let mut flush = |group: &Option<String>, fields: &mut HashMap<String, String>| {
        if let Some(name) = group
            && name != "Icon Theme"
            && let Some(dir) = theme_dir(name, fields)
        {
            dirs.push(dir);
        }
        fields.clear();
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(&group, &mut fields);
            group = Some(name.to_owned());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if group.as_deref() == Some("Icon Theme") && key == "Inherits" {
            inherits.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
        fields.insert(key.to_owned(), value.to_owned());
    }
    flush(&group, &mut fields);

    Theme { dirs, inherits }
}

/// One directory group's fields, as a [`ThemeDir`].
fn theme_dir(path: &str, fields: &HashMap<String, String>) -> Option<ThemeDir> {
    let number = |key: &str| fields.get(key).and_then(|v| v.parse::<u32>().ok());
    // A group with no Size is not a directory group; the spec requires it.
    let size = number("Size")?;
    let kind = match fields.get("Type").map(String::as_str) {
        Some("Fixed") => SizeKind::Fixed,
        Some("Scalable") => SizeKind::Scalable,
        // Threshold is the spec's default when Type is absent or unknown.
        _ => SizeKind::Threshold,
    };
    Some(ThemeDir {
        path: path.to_owned(),
        size,
        scale: number("Scale").unwrap_or(1),
        kind,
        min: number("MinSize").unwrap_or(size),
        max: number("MaxSize").unwrap_or(size),
        threshold: number("Threshold").unwrap_or(2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway icon tree, removed when the test ends.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("raven-icons-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("scratch tree");
            Self(root)
        }

        fn theme(&self, name: &str, index: &str) -> &Self {
            let dir = self.0.join(name);
            std::fs::create_dir_all(&dir).expect("theme dir");
            std::fs::write(dir.join("index.theme"), index).expect("index.theme");
            self
        }

        fn icon(&self, relative: &str) -> &Self {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("icon dir");
            std::fs::write(&path, b"x").expect("icon file");
            self
        }

        fn icons(&self, theme: &str) -> Icons {
            Icons::with_bases(theme, vec![self.0.clone()])
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// hicolor as it is actually installed on this machine.
    const HICOLOR: &str = "\
[Icon Theme]
Name=Hicolor
Directories=48x48/apps,scalable/apps

[48x48/apps]
Size=48
Type=Threshold
Context=Applications

[scalable/apps]
MinSize=1
Size=128
MaxSize=256
Type=Scalable
Context=Applications
";

    #[test]
    fn an_svg_only_icon_is_found_at_a_raster_size() {
        // The RavenTerminal case exactly: the only file on disk is an SVG in
        // a scalable directory, and the launcher asks for 48px. A lookup that
        // only understood raster directories would find nothing.
        let tree = Tree::new("svg-only");
        tree.theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/raven-terminal.svg");

        let found = tree.icons("hicolor").find("raven-terminal", 48, 1);
        assert_eq!(
            found.as_deref(),
            Some(tree.0.join("hicolor/scalable/apps/raven-terminal.svg").as_path()),
            "the scalable SVG was not found at 48px"
        );
    }

    #[test]
    fn the_scalable_range_is_inclusive_at_both_ends() {
        // hicolor declares MinSize=1 MaxSize=256. An off-by-one at either end
        // means an SVG-only theme silently produces no icon.
        let tree = Tree::new("bounds");
        tree.theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");
        let icons = tree.icons("hicolor");
        for size in [1, 2, 128, 255, 256] {
            assert!(icons.find("app", size, 1).is_some(), "no icon at {size}px");
        }
    }

    #[test]
    fn an_exact_raster_match_wins_over_a_scalable_one() {
        let tree = Tree::new("exact");
        tree.theme("hicolor", HICOLOR)
            .icon("hicolor/48x48/apps/app.png")
            .icon("hicolor/scalable/apps/app.svg");
        // 48 is exactly the raster directory's size, so it wins even though
        // SVG sorts first by extension.
        assert_eq!(
            tree.icons("hicolor").find("app", 48, 1),
            Some(tree.0.join("hicolor/48x48/apps/app.png"))
        );
    }

    #[test]
    fn svg_beats_png_within_one_directory() {
        let tree = Tree::new("svg-first");
        tree.theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.png")
            .icon("hicolor/scalable/apps/app.svg");
        assert_eq!(
            tree.icons("hicolor").find("app", 64, 1),
            Some(tree.0.join("hicolor/scalable/apps/app.svg"))
        );
    }

    #[test]
    fn a_theme_falls_back_through_what_it_inherits() {
        let tree = Tree::new("inherit");
        tree.theme(
            "Papyrus",
            "[Icon Theme]\nName=Papyrus\nInherits=hicolor\nDirectories=\n",
        )
        .theme("hicolor", HICOLOR)
        .icon("hicolor/scalable/apps/app.svg");
        assert!(
            tree.icons("Papyrus").find("app", 48, 1).is_some(),
            "inherited theme did not reach hicolor"
        );
    }

    #[test]
    fn hicolor_is_consulted_even_when_nothing_inherits_it() {
        let tree = Tree::new("implicit");
        tree.theme("Bare", "[Icon Theme]\nName=Bare\nDirectories=\n")
            .theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");
        assert!(tree.icons("Bare").find("app", 48, 1).is_some());
    }

    #[test]
    fn an_inheritance_cycle_does_not_hang() {
        // Real themes contain these. A naive walk follows one forever.
        let tree = Tree::new("cycle");
        tree.theme("A", "[Icon Theme]\nInherits=B\nDirectories=\n")
            .theme("B", "[Icon Theme]\nInherits=A\nDirectories=\n")
            .theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");
        assert!(tree.icons("A").find("app", 48, 1).is_some());
    }

    #[test]
    fn an_absolute_icon_path_is_used_directly() {
        let tree = Tree::new("absolute");
        tree.icon("loose/thing.png");
        let path = tree.0.join("loose/thing.png");
        let icons = tree.icons("hicolor");
        assert_eq!(icons.find(path.to_str().expect("utf8"), 48, 1), Some(path));
        // And a path that does not exist is a miss, not a returned bad path.
        assert_eq!(icons.find("/nonexistent/nope.png", 48, 1), None);
    }

    #[test]
    fn an_icon_name_written_with_its_extension_still_resolves() {
        // Against the spec, but common in the wild. Searching for
        // "app.png.svg" would find nothing.
        let tree = Tree::new("extension");
        tree.theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");
        assert!(tree.icons("hicolor").find("app.png", 48, 1).is_some());
    }

    #[test]
    fn a_missing_icon_is_none_rather_than_a_path_that_does_not_exist() {
        let tree = Tree::new("missing");
        tree.theme("hicolor", HICOLOR);
        assert_eq!(tree.icons("hicolor").find("nothing-here", 48, 1), None);
    }

    #[test]
    fn the_nearest_size_is_used_when_nothing_matches_exactly() {
        let tree = Tree::new("nearest");
        tree.theme(
            "sized",
            "[Icon Theme]\nDirectories=16x16/apps,64x64/apps\n\
             [16x16/apps]\nSize=16\nType=Fixed\n\
             [64x64/apps]\nSize=64\nType=Fixed\n",
        )
        .icon("sized/16x16/apps/app.png")
        .icon("sized/64x64/apps/app.png");
        // 48 matches neither; 64 is closer than 16.
        assert_eq!(
            tree.icons("sized").find("app", 48, 1),
            Some(tree.0.join("sized/64x64/apps/app.png"))
        );
    }

    #[test]
    fn a_scaled_directory_is_not_used_for_an_unscaled_request() {
        let tree = Tree::new("scale");
        tree.theme(
            "hidpi",
            "[Icon Theme]\nDirectories=32x32/apps,32x32@2/apps\n\
             [32x32/apps]\nSize=32\nType=Fixed\nScale=1\n\
             [32x32@2/apps]\nSize=32\nType=Fixed\nScale=2\n",
        )
        .icon("hidpi/32x32/apps/app.png")
        .icon("hidpi/32x32@2/apps/app.png");
        let icons = tree.icons("hidpi");
        assert_eq!(icons.find("app", 32, 1), Some(tree.0.join("hidpi/32x32/apps/app.png")));
        assert_eq!(icons.find("app", 32, 2), Some(tree.0.join("hidpi/32x32@2/apps/app.png")));
    }

    #[test]
    fn threshold_is_the_default_type_and_defaults_to_two() {
        let tree = Tree::new("threshold");
        tree.theme(
            "t",
            "[Icon Theme]\nDirectories=48x48/apps\n[48x48/apps]\nSize=48\n",
        )
        .icon("t/48x48/apps/app.png");
        let icons = tree.icons("t");
        // Within the default threshold of 2.
        assert!(icons.find("app", 50, 1).is_some());
        assert!(icons.find("app", 46, 1).is_some());
    }

    #[test]
    fn a_theme_split_across_base_directories_is_one_theme() {
        // How a user overrides a single icon of a system theme.
        let system = Tree::new("split-system");
        let user = Tree::new("split-user");
        system
            .theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");
        user.theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/override.svg");

        // User base first, as default_bases orders them.
        let icons = Icons::with_bases("hicolor", vec![user.0.clone(), system.0.clone()]);
        assert!(icons.find("app", 48, 1).is_some(), "system icon unreachable");
        assert!(icons.find("override", 48, 1).is_some(), "user icon unreachable");
    }

    #[test]
    fn user_base_directories_take_precedence_over_system_ones() {
        let system = Tree::new("prec-system");
        let user = Tree::new("prec-user");
        system
            .theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");
        user.theme("hicolor", HICOLOR)
            .icon("hicolor/scalable/apps/app.svg");

        let icons = Icons::with_bases("hicolor", vec![user.0.clone(), system.0.clone()]);
        assert_eq!(
            icons.find("app", 48, 1),
            Some(user.0.join("hicolor/scalable/apps/app.svg")),
            "the system icon won, so a user override would be ignored"
        );
    }

    #[test]
    fn the_default_bases_put_the_user_first_and_include_pixmaps() {
        let bases = default_bases();
        let position = |needle: &str| bases.iter().position(|b| b.to_string_lossy().contains(needle));
        if let (Some(user), Some(system)) = (position(".local/share/icons"), position("/usr/share/icons")) {
            assert!(user < system, "system icons would shadow user icons");
        }
        assert!(position("pixmaps").is_some(), "legacy pixmaps not searched");
    }
}
