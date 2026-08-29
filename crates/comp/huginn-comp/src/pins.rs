//! What is pinned, in what order, and where the panel that shows it sits.
//!
//! Three things, kept together because they are saved together: the desktop
//! files the user has pinned, the order they put them in, and the position
//! and orientation of the panel — chosen in quick settings — that lays them
//! out. The panel itself is [`crate::pinned`]; this is the part it reads and
//! the part quick settings writes.
//!
//! # Not a configuration file
//!
//! §11 rules out user-facing compositor config, and this is not one: it is
//! written by the desktop, on the user's behalf, when they pin something or
//! step a row in quick settings — exactly as the launch history is written
//! when they launch something. It lives beside that history under
//! `$XDG_STATE_HOME/raven/`, and like it is text that a person could repair
//! with an editor, but nobody is expected to open it. The desktop does not
//! read it to learn how to behave; it reads it to remember what it was told.

use std::path::{Path, PathBuf};

/// Where the pinned panel sits on the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Position {
    #[default]
    Centre,
    Top,
    Bottom,
    Left,
    Right,
}

impl Position {
    /// Every position, in the order quick settings steps through them.
    pub(crate) const ALL: [Self; 5] = [
        Self::Centre,
        Self::Top,
        Self::Bottom,
        Self::Left,
        Self::Right,
    ];

    /// What the quick settings row shows, and what the file says.
    ///
    /// Paired with [`Self::from_value`] and tested to round-trip, for the
    /// reason `IdleAfter::value` gives: the panel stores a control's state
    /// inside the control and reads it back out through this string.
    pub(crate) fn value(self) -> &'static str {
        match self {
            Self::Centre => "Centre",
            Self::Top => "Top",
            Self::Bottom => "Bottom",
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }

    pub(crate) fn from_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.value().eq_ignore_ascii_case(value.trim()))
    }

    /// The next position, `delta` steps along [`Self::ALL`], wrapping.
    pub(crate) fn stepped(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as i32;
        Self::ALL[(at + delta).rem_euclid(n) as usize]
    }
}

/// How the pinned panel lays its applications out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Orientation {
    /// Tiles, several to a row, like the launcher's suggestions.
    #[default]
    Grid,
    /// One row of tiles, side by side. A strip; what a dock would show.
    Row,
    /// One column of rows, like the launcher's results.
    Column,
}

impl Orientation {
    pub(crate) const ALL: [Self; 3] = [Self::Grid, Self::Row, Self::Column];

    pub(crate) fn value(self) -> &'static str {
        match self {
            Self::Grid => "Grid",
            Self::Row => "Row",
            Self::Column => "Column",
        }
    }

    pub(crate) fn from_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.value().eq_ignore_ascii_case(value.trim()))
    }

    pub(crate) fn stepped(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|o| *o == self).unwrap_or(0) as i32;
        Self::ALL[(at + delta).rem_euclid(n) as usize]
    }
}

/// The pin list and the panel's layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Pins {
    /// Desktop files, in the order they are shown. A path that resolves to
    /// no installed application is kept rather than dropped: the
    /// application may be reinstalled, and a pin that vanished because a
    /// package was briefly absent is a pin the user has to find again.
    paths: Vec<PathBuf>,
    position: Position,
    orientation: Orientation,
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

    pub(crate) fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Returns whether anything changed.
    pub(crate) fn set_position(&mut self, position: Position) -> bool {
        let changed = self.position != position;
        self.position = position;
        changed
    }

    /// Returns whether anything changed.
    pub(crate) fn set_orientation(&mut self, orientation: Orientation) -> bool {
        let changed = self.orientation != orientation;
        self.orientation = orientation;
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
    /// By path rather than by index, because the panel shows only the pins
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
        let mut text = format!(
            "position\t{}\norientation\t{}\n",
            self.position.value(),
            self.orientation.value()
        );
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
    /// that means nothing, a position that is not one of the five — each is
    /// skipped, and what the rest of the file says is kept. A pin listed
    /// twice is kept once, in its first place.
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
                    }
                }
                "orientation" => {
                    if let Some(orientation) = Orientation::from_value(value) {
                        pins.orientation = orientation;
                    }
                }
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
    fn the_text_round_trips_including_order_and_layout() {
        let mut pins = Pins::new();
        pins.pin(&p("zed"));
        pins.pin(&p("alpha"));
        pins.set_position(Position::Bottom);
        pins.set_orientation(Orientation::Row);
        let back = Pins::parse(&pins.to_text());
        assert_eq!(back, pins);
        assert_eq!(back.paths(), [p("zed"), p("alpha")], "order was lost");
    }

    #[test]
    fn parsing_forgives_what_it_does_not_understand() {
        let text = "# a comment\n\nposition\tSideways\norientation\trow\nwhat\tever\npin\t/x.desktop\npin\t/x.desktop\nno tab here\n";
        let pins = Pins::parse(text);
        assert_eq!(
            pins.position(),
            Position::Centre,
            "a bad position was not ignored"
        );
        assert_eq!(
            pins.orientation(),
            Orientation::Row,
            "case should not matter"
        );
        assert_eq!(pins.paths(), [PathBuf::from("/x.desktop")]);
    }

    #[test]
    fn every_value_round_trips_through_its_label() {
        for position in Position::ALL {
            assert_eq!(Position::from_value(position.value()), Some(position));
        }
        for orientation in Orientation::ALL {
            assert_eq!(
                Orientation::from_value(orientation.value()),
                Some(orientation)
            );
        }
    }

    #[test]
    fn stepping_wraps_both_ways() {
        assert_eq!(Position::Centre.stepped(-1), Position::Right);
        assert_eq!(Position::Right.stepped(1), Position::Centre);
        assert_eq!(Orientation::Grid.stepped(1), Orientation::Row);
        assert_eq!(Orientation::Grid.stepped(-1), Orientation::Column);
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
