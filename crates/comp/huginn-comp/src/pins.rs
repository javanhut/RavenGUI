//! What is pinned, in what order, and which edge the bar that shows it rides.
//!
//! Three things, kept together because they are saved together: the desktop
//! files the user has pinned, the order they put them in, and the edge —
//! chosen in Raven Settings — that the bar rides. The bar itself is
//! [`crate::pinned`]; this is the part it reads and the part Raven Settings
//! writes.
//!
//! # One control, not two
//!
//! There used to be a position of five (including a floating centre) and an
//! orientation of three (grid, row, column), which is fifteen layouts for a
//! thing that is one icon wide. The pin bar is a rail against an edge, and an
//! edge already says which way a rail runs: down the screen on the left or
//! right, across it on the top or bottom. So there is one control, and
//! [`Position::is_vertical`] is the whole of what used to be `Orientation`.
//!
//! A file written by a release that had both is still read: a position of
//! `Centre` becomes the default edge, and an `orientation` line is read and
//! dropped rather than treated as a line nobody understands. See
//! [`Pins::parse`].
//!
//! # Not a configuration file
//!
//! §11 rules out user-facing compositor config, and this is not one: it is
//! written by the desktop, on the user's behalf, when they pin something or
//! choose an edge in Raven Settings — exactly as the launch history is
//! written when they launch something. It lives beside that history under
//! `$XDG_STATE_HOME/raven/`, and like it is text that a person could repair
//! with an editor, but nobody is expected to open it. The desktop does not
//! read it to learn how to behave; it reads it to remember what it was told.

use std::path::{Path, PathBuf};

/// Which edge of the output the pin bar rides.
///
/// The default is the right edge: a rail down the right is clear of the dock
/// at the bottom and of a maximised window's title bar at the top, and it is
/// the side a right-handed pointer arrives from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Position {
    #[default]
    Right,
    Left,
    Top,
    Bottom,
}

impl Position {
    /// Every edge, in the order Raven Settings and quick settings step
    /// through them: the default first.
    pub(crate) const ALL: [Self; 4] = [Self::Right, Self::Left, Self::Top, Self::Bottom];

    /// What the settings row shows, and what the file says.
    ///
    /// Paired with [`Self::from_value`] and tested to round-trip, for the
    /// reason `IdleAfter::value` gives: the panel stores a control's state
    /// inside the control and reads it back out through this string.
    pub(crate) fn value(self) -> &'static str {
        match self {
            Self::Right => "Right",
            Self::Left => "Left",
            Self::Top => "Top",
            Self::Bottom => "Bottom",
        }
    }

    pub(crate) fn from_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.value().eq_ignore_ascii_case(value.trim()))
    }

    /// The next edge, `delta` steps along [`Self::ALL`], wrapping.
    pub(crate) fn stepped(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as i32;
        Self::ALL[(at + delta).rem_euclid(n) as usize]
    }

    /// Whether the bar runs down the screen rather than across it.
    ///
    /// The edge decides: a rail on the left or the right is a column, one on
    /// the top or the bottom is a row. Everything the bar draws and every
    /// arrow key it answers comes from this one question.
    pub(crate) fn is_vertical(self) -> bool {
        matches!(self, Self::Right | Self::Left)
    }
}

/// The pin list and which edge shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Pins {
    /// Desktop files, in the order they are shown. A path that resolves to
    /// no installed application is kept rather than dropped: the
    /// application may be reinstalled, and a pin that vanished because a
    /// package was briefly absent is a pin the user has to find again.
    paths: Vec<PathBuf>,
    position: Position,
}

impl Pins {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// What is pinned, in order.
    pub(crate) fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub(crate) fn is_pinned(&self, path: &Path) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    pub(crate) fn position(&self) -> Position {
        self.position
    }

    /// Returns whether anything changed.
    pub(crate) fn set_position(&mut self, position: Position) -> bool {
        let changed = self.position != position;
        self.position = position;
        changed
    }

    /// Pin `path` at the end, unless it already is. Returns whether it was
    /// added.
    pub(crate) fn pin(&mut self, path: &Path) -> bool {
        if self.is_pinned(path) {
            return false;
        }
        self.paths.push(path.to_path_buf());
        true
    }

    /// Unpin `path`. Returns whether it was there.
    pub(crate) fn unpin(&mut self, path: &Path) -> bool {
        let before = self.paths.len();
        self.paths.retain(|p| p != path);
        self.paths.len() != before
    }

    /// Pin `path` if it is not pinned, unpin it if it is. Returns whether it
    /// is pinned now.
    pub(crate) fn toggle(&mut self, path: &Path) -> bool {
        if self.unpin(path) {
            false
        } else {
            self.pin(path);
            true
        }
    }

    /// Move `path` to just before `other`, or just after it when `after`.
    ///
    /// By path rather than by index, because the bar shows only the pins
    /// that resolve to an installed application — its neighbour on screen
    /// may be several unresolved pins away in this list, and "put this one
    /// where that one is" is what the user asked for in either case. Returns
    /// whether anything moved.
    pub(crate) fn place(&mut self, path: &Path, other: &Path, after: bool) -> bool {
        if path == other || !self.is_pinned(path) || !self.is_pinned(other) {
            return false;
        }
        let before: Vec<PathBuf> = self.paths.clone();
        self.paths.retain(|p| p != path);
        let Some(at) = self.paths.iter().position(|p| p == other) else {
            self.paths = before;
            return false;
        };
        self.paths
            .insert(at + usize::from(after), path.to_path_buf());
        self.paths != before
    }

    /// The file's text.
    ///
    /// Tab-separated, one fact per line, and the pins in the order they are
    /// shown: this is the one file in the state directory whose line order
    /// means something, so it is not sorted. A key first so that a line can
    /// be recognised by what it is, and the path last so that it can be
    /// anything at all — the same shape as the launch history.
    pub(crate) fn to_text(&self) -> String {
        let mut text = format!("position\t{}\n", self.position.value());
        for path in &self.paths {
            text.push_str("pin\t");
            text.push_str(&path.to_string_lossy());
            text.push('\n');
        }
        text
    }

    /// Read back what [`Self::to_text`] wrote.
    ///
    /// Forgiving, as the launch history is: a blank line, a comment, a key
    /// that means nothing, a position that is not one of the four — each is
    /// skipped, and what the rest of the file says is kept. A pin listed
    /// twice is kept once, in its first place.
    ///
    /// Two lines are forgiven by name rather than by accident, because a
    /// release that shipped them wrote them deliberately: `Centre`, which
    /// was a fifth position when the bar floated, becomes the default edge,
    /// and `orientation`, which was a second control before the edge decided
    /// which way the rail runs, is dropped. See the module documentation.
    pub(crate) fn parse(text: &str) -> Self {
        let mut pins = Self::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('\t') else {
                continue;
            };
            match key.trim() {
                "position" => {
                    if let Some(position) = Position::from_value(value) {
                        pins.position = position;
                    } else if matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "centre" | "center"
                    ) {
                        pins.position = Position::default();
                    }
                }
                "orientation" => {}
                "pin" if !value.is_empty() => {
                    pins.pin(Path::new(value));
                }
                _ => {}
            }
        }
        pins
    }

    /// Load from `path`, treating a missing file as nothing pinned.
    pub(crate) fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }

    /// Write to `path`, creating its directory, without ever leaving a
    /// half-written file behind. The same sibling-and-rename as the launch
    /// history, for the same reason: a truncated file is every pin gone.
    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "pins".to_owned());
        let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let result =
            std::fs::write(&temp, self.to_text()).and_then(|()| std::fs::rename(&temp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!("/apps/{name}.desktop"))
    }

    #[test]
    fn pinning_appends_and_toggling_removes() {
        let mut pins = Pins::new();
        assert!(pins.toggle(&p("a")));
        assert!(pins.toggle(&p("b")));
        assert!(!pins.pin(&p("a")), "pinned twice");
        assert_eq!(pins.paths(), [p("a"), p("b")]);
        assert!(!pins.toggle(&p("a")));
        assert_eq!(pins.paths(), [p("b")]);
        assert!(!pins.unpin(&p("zzz")));
    }

    #[test]
    fn placing_moves_a_pin_beside_another() {
        let mut pins = Pins::new();
        for name in ["a", "b", "c", "d"] {
            pins.pin(&p(name));
        }
        assert!(pins.place(&p("a"), &p("c"), true));
        assert_eq!(pins.paths(), [p("b"), p("c"), p("a"), p("d")]);
        assert!(pins.place(&p("d"), &p("b"), false));
        assert_eq!(pins.paths(), [p("d"), p("b"), p("c"), p("a")]);
        // Onto itself, or beside something not pinned: nothing.
        assert!(!pins.place(&p("a"), &p("a"), true));
        assert!(!pins.place(&p("a"), &p("nope"), true));
        assert_eq!(pins.paths(), [p("d"), p("b"), p("c"), p("a")]);
    }

    #[test]
    fn the_text_round_trips_including_order_and_edge() {
        let mut pins = Pins::new();
        pins.pin(&p("zed"));
        pins.pin(&p("alpha"));
        pins.set_position(Position::Bottom);
        let back = Pins::parse(&pins.to_text());
        assert_eq!(back, pins);
        assert_eq!(back.paths(), [p("zed"), p("alpha")], "order was lost");
    }

    #[test]
    fn parsing_forgives_what_it_does_not_understand() {
        let text = "# a comment\n\nposition\tSideways\nwhat\tever\npin\t/x.desktop\npin\t/x.desktop\nno tab here\n";
        let pins = Pins::parse(text);
        assert_eq!(
            pins.position(),
            Position::Right,
            "a bad position was not ignored"
        );
        assert_eq!(pins.paths(), [PathBuf::from("/x.desktop")]);
        // Case does not matter for one it does understand.
        assert_eq!(
            Pins::parse("position\tbottom\n").position(),
            Position::Bottom
        );
    }

    /// A file from the release that had five positions and three layouts.
    #[test]
    fn a_file_from_when_there_were_two_controls_still_reads() {
        let text = "position\tCentre\norientation\tGrid\npin\t/x.desktop\n";
        let pins = Pins::parse(text);
        assert_eq!(
            pins.position(),
            Position::Right,
            "the floating centre did not become an edge"
        );
        assert_eq!(pins.paths(), [PathBuf::from("/x.desktop")]);
        // An edge that survived the change keeps its meaning, and the
        // orientation beside it is dropped rather than confusing it.
        let pins = Pins::parse("position\tLeft\norientation\tColumn\n");
        assert_eq!(pins.position(), Position::Left);
        assert!(!pins.to_text().contains("orientation"));
    }

    #[test]
    fn every_edge_round_trips_through_its_label() {
        for position in Position::ALL {
            assert_eq!(Position::from_value(position.value()), Some(position));
        }
    }

    #[test]
    fn the_edge_decides_which_way_the_rail_runs() {
        assert!(Position::Right.is_vertical() && Position::Left.is_vertical());
        assert!(!Position::Top.is_vertical() && !Position::Bottom.is_vertical());
    }

    #[test]
    fn stepping_wraps_both_ways() {
        assert_eq!(Position::Right.stepped(-1), Position::Bottom);
        assert_eq!(Position::Bottom.stepped(1), Position::Right);
        assert_eq!(Position::Right.stepped(1), Position::Left);
    }

    #[test]
    fn saving_and_loading_a_file() {
        let dir = std::env::temp_dir().join(format!("raven-pins-test-{}", std::process::id()));
        let file = dir.join("nested").join("pins");
        let mut pins = Pins::new();
        pins.pin(&p("a"));
        pins.save(&file).expect("save");
        assert_eq!(Pins::load(&file).expect("load"), pins);
        assert_eq!(
            Pins::load(&dir.join("missing")).expect("missing is empty"),
            Pins::new()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
