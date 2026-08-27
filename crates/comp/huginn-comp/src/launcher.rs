//! The application launcher's state and editing model.
//!
//! Search-first: the field is focused the moment it opens, two characters and
//! Return should be enough. Everything here is the part that decides what a
//! keystroke means and which application is selected — no drawing, no Wayland,
//! no spawning. Those live in the compositor; this is the half worth testing.
//!
//! # Why the compositor takes every key
//!
//! The launcher is drawn by the compositor, not by a client, so there is no
//! surface to give keyboard focus to and no client to forward to. While it is
//! open the keymap stops resolving chords and hands every key here instead —
//! see [`Key::from_keysym`] and the launcher branch of
//! `crate::backend::keymap::resolve`. That is also why `Escape` matters so
//! much: it is the only way back out, and a launcher that swallowed keys with
//! no exit would take the keyboard away from the session entirely.

use raven_desktop::{Entry, Frecency, Icons, Pixmaps, entry, search};

/// Read every installed application.
///
/// Called at startup and again whenever [`crate::appwatch`] sees one of these
/// directories change, which is what lets a freshly installed application
/// appear without a restart. §4.
///
/// Directories are visited in precedence order and entries shadow by file
/// name, so a user's copy of an application replaces the system's rather than
/// joining it. Both halves come from [`entry`], which is also what
/// [`crate::appwatch`] watches — the scan and the watch reading the same list
/// from the same place is what stops them disagreeing about where an
/// application may come from.
pub(crate) fn scan_applications() -> Vec<Entry> {
    let current: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let mut apps = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for dir in entry::directories() {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            // Before reading the file, not after: a shadowing entry that
            // `parse` rejects — `Hidden=true`, the spec's way to remove an
            // application — must still suppress the copy it shadows.
            if entry::shadows(&mut seen, &path) {
                tracing::debug!(path = %path.display(), "shadowed by a higher-precedence entry");
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(app) = entry::parse(&text, &path, &current) {
                apps.push(app);
            }
        }
    }

    // `read_dir` yields whatever order the filesystem feels like, which makes
    // the dock's trailing run of applications reshuffle between logins and
    // makes two scans of an unchanged system compare unequal. Sorting by path
    // costs nothing at this size and removes both.
    apps.sort_by(|a, b| a.path.cmp(&b.path));
    tracing::info!(count = apps.len(), "applications indexed");
    apps
}

/// What a keystroke means to the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// A character to append to the query.
    Insert(char),
    /// Delete the character before the cursor.
    Backspace,
    /// Delete the word before the cursor.
    DeleteWord,
    /// Empty the query, keeping the launcher open.
    Clear,
    /// Move the selection.
    Up,
    Down,
    /// Launch the selected application.
    Launch,
    /// Close without launching.
    Dismiss,
    /// Recognised, deliberately does nothing. Distinct from "not a key we
    /// know" so that the caller can still swallow it: a modifier press
    /// reaching the focused client while the launcher is open would let a
    /// window act on a chord the user was typing at the launcher.
    Ignored,
}

impl Key {
    /// Interpret a keysym and modifiers as a launcher key.
    ///
    /// Takes the character from the keysym rather than mapping symbols to
    /// letters here, so a Dvorak or AZERTY layout types what its user expects.
    /// A hardcoded `KEY_a => 'a'` is correct on exactly one layout.
    pub(crate) fn from_keysym(sym: u32, ctrl: bool, character: Option<char>) -> Self {
        use smithay::input::keyboard::keysyms;
        match sym {
            keysyms::KEY_Escape => Self::Dismiss,
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => Self::Launch,
            keysyms::KEY_BackSpace if ctrl => Self::DeleteWord,
            keysyms::KEY_BackSpace => Self::Backspace,
            keysyms::KEY_Up => Self::Up,
            keysyms::KEY_Down => Self::Down,
            // Ctrl+U empties the line, as it does in every readline-shaped
            // thing anyone has typed into.
            keysyms::KEY_u | keysyms::KEY_U if ctrl => Self::Clear,
            keysyms::KEY_w | keysyms::KEY_W if ctrl => Self::DeleteWord,
            // Emacs-style, because a launcher is a text field and the people
            // most likely to use one by keyboard expect these.
            keysyms::KEY_p | keysyms::KEY_P if ctrl => Self::Up,
            keysyms::KEY_n | keysyms::KEY_N if ctrl => Self::Down,
            _ => match character {
                // A control chord that is not one of the above must not be
                // typed into the query as a stray character.
                Some(c) if !ctrl && !c.is_control() => Self::Insert(c),
                _ => Self::Ignored,
            },
        }
    }
}

/// What the compositor should do after a keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing visible changed; do not redraw.
    Unchanged,
    /// Redraw the launcher.
    Redraw,
    /// Close the launcher without running anything.
    Dismissed,
    /// Close it and run this argv.
    Launch(Vec<String>),
}

/// The launcher.
#[derive(Debug)]
pub(crate) struct Launcher {
    open: bool,
    query: String,
    /// Index into [`Self::results`], not into the application list.
    selected: usize,
    /// Indices into the application list, best match first.
    results: Vec<usize>,
    /// 0 fully collapsed onto its origin, 1 fully open. §4: fade and scale up
    /// from the dock icon over ~150ms, and reverse the same motion to dismiss.
    reveal: crate::anim::Animated,
    /// Where it grows from, in output coordinates.
    ///
    /// The dock's launcher icon when the dock is up. Captured at open rather
    /// than looked up while drawing, because the dock hides itself as soon as
    /// the pointer leaves — and a panel that changed where it was growing from
    /// halfway through would be worse than one that grew from the wrong place.
    origin: Option<Rect>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            results: Vec::new(),
            reveal: crate::anim::Animated::settled(0.0),
            origin: None,
        }
    }
}

impl Launcher {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Read by the drawing code, which does not exist yet — the launcher
    /// currently has input and no panel. Removing this would mean writing it
    /// again alongside the renderer.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn results(&self) -> &[usize] {
        &self.results
    }

    /// Which entry is highlighted, as an index into the application list.
    pub(crate) fn selection(&self) -> Option<usize> {
        self.results.get(self.selected).copied()
    }

    /// Open with an empty query, showing the most-used applications.
    ///
    /// `origin` is the dock's launcher icon, or `None` to grow in place from
    /// the centre — which is what a keyboard shortcut gets when the dock is
    /// hidden. Scaling up from off the bottom of the screen because the dock
    /// happens to be away would be motion the user cannot follow.
    pub(crate) fn open(
        &mut self,
        entries: &[Entry],
        frecency: &Frecency,
        now: u64,
        origin: Option<Rect>,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.origin = origin;
        self.refresh(entries, frecency, now);
        self.reveal.animate_to(
            1.0,
            clock,
            motion.duration(crate::anim::LAUNCHER_OPEN),
            crate::anim::Curve::EaseOut,
        );
    }

    /// Dismiss it, reversing the motion it arrived with. §4.
    pub(crate) fn close(&mut self, clock: std::time::Duration, motion: crate::settings::Motion) {
        self.open = false;
        self.reveal.animate_to(
            0.0,
            clock,
            motion.duration(crate::anim::PANEL_CLOSE),
            crate::anim::Curve::EaseOut,
        );
        // The query is deliberately NOT cleared here: the panel is still on
        // screen shrinking away, and emptying it mid-animation would show the
        // placeholder for the last few frames of a dismissal.
        self.selected = 0;
    }

    /// How far open it is, 0..=1.
    pub(crate) fn reveal(&self, clock: std::time::Duration) -> f32 {
        self.reveal.value(clock)
    }

    pub(crate) fn is_visible(&self, clock: std::time::Duration) -> bool {
        self.open || self.reveal(clock) > 0.001
    }

    pub(crate) fn is_animating(&self, clock: std::time::Duration) -> bool {
        !self.reveal.is_settled(clock)
    }

    /// Where it grows from.
    pub(crate) fn origin(&self) -> Option<Rect> {
        self.origin
    }

    /// Apply a keystroke.
    pub(crate) fn press(
        &mut self,
        key: Key,
        entries: &[Entry],
        frecency: &Frecency,
        now: u64,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        match key {
            Key::Dismiss => {
                self.close(clock, motion);
                Outcome::Dismissed
            }
            Key::Launch => match self.selection().and_then(|i| entries.get(i)) {
                // No targets: the launcher opens applications, not documents.
                Some(entry) => match entry.argv(&[]) {
                    Some(argv) => {
                        self.close(clock, motion);
                        Outcome::Launch(argv)
                    }
                    // An entry whose Exec resolves to nothing must not close
                    // the launcher — silently doing nothing would look like the
                    // key was ignored.
                    None => Outcome::Unchanged,
                },
                // Return on an empty result set does nothing rather than
                // closing, so a typo does not dismiss what you were typing.
                None => Outcome::Unchanged,
            },
            Key::Up => self.move_selection(-1),
            Key::Down => self.move_selection(1),
            Key::Insert(c) => {
                self.query.push(c);
                self.after_edit(entries, frecency, now)
            }
            Key::Backspace => {
                if self.query.pop().is_none() {
                    return Outcome::Unchanged;
                }
                self.after_edit(entries, frecency, now)
            }
            Key::DeleteWord => {
                if self.query.is_empty() {
                    return Outcome::Unchanged;
                }
                let trimmed = self.query.trim_end();
                let cut = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.query.truncate(cut);
                self.after_edit(entries, frecency, now)
            }
            Key::Clear => {
                if self.query.is_empty() {
                    return Outcome::Unchanged;
                }
                self.query.clear();
                self.after_edit(entries, frecency, now)
            }
            Key::Ignored => Outcome::Unchanged,
        }
    }

    /// Re-rank after the query changed, and put the selection back on top.
    ///
    /// Resetting to the first result is the whole point of a search-first
    /// launcher: every keystroke is a new question, and the answer is the best
    /// match, not whatever happened to be highlighted for the previous query.
    fn after_edit(&mut self, entries: &[Entry], frecency: &Frecency, now: u64) -> Outcome {
        self.selected = 0;
        self.refresh(entries, frecency, now);
        Outcome::Redraw
    }

    /// Re-rank against a changed application list, keeping the query.
    ///
    /// Results are indices into that list, so a list that changed underneath
    /// an open launcher leaves the highlight pointing at whatever now sits at
    /// that index — the wrong application, and it would be the one Enter
    /// launches. Re-ranking is what keeps the indices meaning what they say.
    pub(crate) fn reindex(&mut self, entries: &[Entry], frecency: &Frecency, now: u64) {
        self.refresh(entries, frecency, now);
    }

    fn refresh(&mut self, entries: &[Entry], frecency: &Frecency, now: u64) {
        self.results = search(entries, &self.query, frecency, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        // Results can shrink under a selection that was valid a keystroke ago.
        // Clamping here rather than at every read is what stops `selection`
        // pointing past the end and the highlight vanishing.
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    /// Move the highlight, stopping at the ends rather than wrapping.
    ///
    /// Not wrapping is deliberate: the list is ordered best-first, so falling
    /// off the bottom onto the best match again would move the highlight the
    /// furthest possible distance for one keypress.
    fn move_selection(&mut self, delta: isize) -> Outcome {
        if self.results.is_empty() {
            return Outcome::Unchanged;
        }
        let last = self.results.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        if next == self.selected {
            return Outcome::Unchanged;
        }
        self.selected = next;
        Outcome::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, exec: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            comment: None,
            generic_name: None,
            icon: None,
            exec: exec.to_owned(),
            categories: Vec::new(),
            keywords: Vec::new(),
            terminal: false,
            startup_wm_class: None,
            path: PathBuf::from(format!("/apps/{name}.desktop")),
        }
    }

    pub(super) fn apps() -> Vec<Entry> {
        vec![
            entry("Firefox", "/bin/firefox"),
            entry("Files", "/bin/files"),
            entry("Fractal", "/bin/fractal"),
            entry("Raven Terminal", "/bin/raven-terminal %F"),
        ]
    }

    pub(super) const NOW: u64 = 1_700_000_000;
    /// Tests drive the state machine, not the motion; reduced motion keeps
    /// every reveal instant so a test never has to wait for one.
    pub(super) const STILL: crate::settings::Motion = crate::settings::Motion::Reduced;
    pub(super) const CLOCK: std::time::Duration = std::time::Duration::ZERO;

    /// Open a launcher over `apps()` and type `query`.
    fn typed(query: &str) -> (Launcher, Vec<Entry>) {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        (launcher, apps)
    }

    fn selected_name(launcher: &Launcher, apps: &[Entry]) -> Option<String> {
        launcher.selection().map(|i| apps[i].name.clone())
    }

    #[test]
    fn opening_shows_everything_and_selects_the_first() {
        let apps = apps();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &Frecency::new(), NOW, None, CLOCK, STILL);
        assert!(launcher.is_open());
        assert_eq!(launcher.results().len(), apps.len());
        assert!(launcher.selection().is_some());
    }

    #[test]
    fn two_characters_narrow_to_the_right_application() {
        // The acceptance criterion, as far as this half of it goes.
        let (launcher, apps) = typed("ra");
        assert_eq!(selected_name(&launcher, &apps).as_deref(), Some("Raven Terminal"));
    }

    #[test]
    fn typing_moves_the_selection_back_to_the_best_match() {
        // Every keystroke is a new question. Leaving the highlight where it
        // was answers the previous one.
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Insert('f'), &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), launcher.results().first().copied());
    }

    #[test]
    fn the_selection_never_points_past_a_list_that_shrank() {
        // Type to narrow the list while the highlight is near the bottom. A
        // stale index here is a highlight drawn off the end of the panel.
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for _ in 0..3 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        for c in "raven".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert!(launcher.results().len() < apps.len(), "the list did not narrow");
        assert!(launcher.selection().is_some(), "the highlight fell off the list");
    }

    #[test]
    fn the_selection_stops_at_the_ends_rather_than_wrapping() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);

        assert_eq!(launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL), Outcome::Unchanged);
        assert_eq!(launcher.selection(), launcher.results().first().copied());

        for _ in 0..20 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.selection(), launcher.results().last().copied());
        assert_eq!(launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL), Outcome::Unchanged);
    }

    #[test]
    fn escape_closes_without_launching() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Dismissed
        );
        assert!(!launcher.is_open());
        assert!(launcher.query().is_empty(), "the query outlived the launcher");
    }

    #[test]
    fn return_launches_the_selection_and_closes() {
        let (mut launcher, apps) = typed("fire");
        let outcome = launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(outcome, Outcome::Launch(vec!["/bin/firefox".to_owned()]));
        assert!(!launcher.is_open());
    }

    #[test]
    fn launching_strips_field_codes() {
        // The argv comes from Entry::argv, so `%F` with nothing to open must
        // not reach the command line as a literal argument.
        let (mut launcher, apps) = typed("raven");
        let outcome = launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(outcome, Outcome::Launch(vec!["/bin/raven-terminal".to_owned()]));
    }

    #[test]
    fn return_with_no_results_does_not_close_the_launcher() {
        // A typo should not dismiss what you were in the middle of typing.
        let (mut launcher, apps) = typed("qqzz");
        assert!(launcher.results().is_empty());
        assert_eq!(
            launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert!(launcher.is_open());
    }

    #[test]
    fn backspace_widens_the_search_again() {
        let apps = apps();
        let frecency = Frecency::new();
        let (mut launcher, _) = typed("raven");
        let narrow = launcher.results().len();
        for _ in 0..5 {
            launcher.press(Key::Backspace, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert!(launcher.query().is_empty());
        assert!(launcher.results().len() > narrow);
    }

    #[test]
    fn backspace_on_an_empty_query_is_not_a_redraw() {
        // Repainting the whole launcher because a key did nothing is how a
        // search field ends up feeling laggy.
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Backspace, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn delete_word_removes_one_word_at_a_time() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in "raven term".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        launcher.press(Key::DeleteWord, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.query(), "raven ");
        launcher.press(Key::DeleteWord, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.query(), "");
    }

    #[test]
    fn clear_empties_the_query_but_leaves_the_launcher_open() {
        let apps = apps();
        let (mut launcher, _) = typed("firefox");
        launcher.press(Key::Clear, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert!(launcher.query().is_empty());
        assert!(launcher.is_open(), "clearing should not dismiss");
        assert_eq!(launcher.results().len(), apps.len());
    }

    #[test]
    fn a_closed_launcher_ignores_every_key() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        for key in [Key::Insert('a'), Key::Launch, Key::Down, Key::Dismiss] {
            assert_eq!(launcher.press(key, &apps, &frecency, NOW, CLOCK, STILL), Outcome::Unchanged);
        }
        assert!(!launcher.is_open());
    }

    #[test]
    fn reopening_starts_from_a_clean_query() {
        let apps = apps();
        let frecency = Frecency::new();
        let (mut launcher, _) = typed("firefox");
        launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert!(launcher.query().is_empty(), "the last search came back");
        assert_eq!(launcher.results().len(), apps.len());
    }

    // ---- key mapping ----

    use smithay::input::keyboard::keysyms;

    #[test]
    fn characters_come_from_the_layout_not_from_the_keysym() {
        // Mapping KEY_a to 'a' here is correct on exactly one layout. The
        // character the keymap produced is what gets typed.
        assert_eq!(Key::from_keysym(keysyms::KEY_a, false, Some('a')), Key::Insert('a'));
        assert_eq!(Key::from_keysym(keysyms::KEY_a, false, Some('ä')), Key::Insert('ä'));
    }

    #[test]
    fn control_chords_never_leak_a_character_into_the_query() {
        // Ctrl+S has a character on some layouts. Typing an 's' because of it
        // would corrupt the query on a keystroke meant for something else.
        assert_eq!(Key::from_keysym(keysyms::KEY_s, true, Some('s')), Key::Ignored);
    }

    #[test]
    fn the_editing_chords_are_the_ones_people_already_know() {
        assert_eq!(Key::from_keysym(keysyms::KEY_u, true, Some('u')), Key::Clear);
        assert_eq!(Key::from_keysym(keysyms::KEY_w, true, Some('w')), Key::DeleteWord);
        assert_eq!(Key::from_keysym(keysyms::KEY_p, true, Some('p')), Key::Up);
        assert_eq!(Key::from_keysym(keysyms::KEY_n, true, Some('n')), Key::Down);
        assert_eq!(
            Key::from_keysym(keysyms::KEY_BackSpace, true, None),
            Key::DeleteWord
        );
    }

    #[test]
    fn escape_and_return_are_recognised_without_a_character() {
        assert_eq!(Key::from_keysym(keysyms::KEY_Escape, false, None), Key::Dismiss);
        assert_eq!(Key::from_keysym(keysyms::KEY_Return, false, None), Key::Launch);
        assert_eq!(Key::from_keysym(keysyms::KEY_KP_Enter, false, None), Key::Launch);
    }

    #[test]
    fn a_control_character_is_never_typed_into_the_query() {
        // A keysym can carry \t or \r as its character; appending either would
        // put an invisible character in the search field.
        for c in ['\t', '\r', '\n', '\u{7f}'] {
            assert_eq!(Key::from_keysym(keysyms::KEY_a, false, Some(c)), Key::Ignored);
        }
    }

    #[test]
    fn an_unknown_key_with_no_character_is_ignored_rather_than_forwarded() {
        // Still swallowed by the caller: a modifier press reaching the focused
        // client while the launcher is open lets a window act on a chord the
        // user was typing at the launcher.
        assert_eq!(Key::from_keysym(keysyms::KEY_Shift_L, false, None), Key::Ignored);
    }

    #[test]
    fn reindexing_keeps_the_highlight_on_the_application_it_was_on() {
        // Results are indices into the application list. An install shifts
        // every index after it, so a launcher left un-reindexed highlights one
        // application and launches the one that took its place.
        let (mut launcher, apps) = typed("fi");
        let chosen = apps[launcher.selection().expect("a selection")].name.clone();

        let mut grown = apps.clone();
        grown.insert(0, entry("Aardvark", "/bin/aardvark"));

        launcher.reindex(&grown, &Frecency::new(), NOW);
        assert_eq!(
            grown[launcher.selection().expect("still a selection")].name,
            chosen,
        );
    }

    #[test]
    fn reindexing_keeps_the_query() {
        // Re-ranking is not reopening: whatever the user has typed survives an
        // install that happens while they are typing it.
        let (mut launcher, apps) = typed("fi");
        let before = launcher.results.len();

        launcher.reindex(&apps, &Frecency::new(), NOW);
        assert_eq!(launcher.results.len(), before);
        assert!(
            launcher.results.iter().all(|i| apps[*i].name.starts_with("Fi")
                || apps[*i].name.contains("fi")
                || apps[*i].name.starts_with("Fr")),
            "the query was lost and everything matched",
        );
    }

    #[test]
    fn reindexing_after_a_removal_leaves_no_selection_past_the_end() {
        let (mut launcher, apps) = typed("");
        launcher.press(Key::Down, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        launcher.press(Key::Down, &apps, &Frecency::new(), NOW, CLOCK, STILL);

        // Everything the user could have highlighted is uninstalled at once.
        let empty: Vec<Entry> = Vec::new();
        launcher.reindex(&empty, &Frecency::new(), NOW);
        assert_eq!(launcher.selection(), None);
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

use crate::canvas::{Canvas, Panel};
use crate::text::Text;
use huginn_core::geometry::Rect;

/// Panel width at a 1080p output, in pixels.
const WIDTH: f32 = 560.0;
/// Padding inside the panel's border.
const PAD: f32 = 18.0;
/// Text size at a 1080p output.
const BASE_SIZE: f32 = 16.0;
/// How many results are shown. Beyond this the answer was not in the list and
/// another keystroke is faster than another screenful.
const VISIBLE: usize = 8;
/// Opacity of the panel's background.
const ALPHA: u8 = 0xF2;

/// What is shown before anything has been typed.
const PLACEHOLDER: &str = "Search applications";

/// Where the panel sits, and how big, at the current reveal.
///
/// Interpolates from `origin` — the dock's launcher icon — to the panel at
/// full size, centred. The size is interpolated too, so the renderer scales
/// the finished buffer rather than the panel being composed again per frame:
/// re-shaping eight rows of text sixty times a second to animate a scale would
/// cost more than the animation is worth, and the glyphs would shimmer as they
/// re-hinted at each intermediate size.
///
/// Never scales below [`MIN_SCALE`] of full size. Growing from the icon's
/// literal 44 pixels means the first frames are an unreadable smear, and the
/// eye reads the motion, not the content, at that point anyway.
pub(crate) fn placement(output: Rect, panel: (i32, i32), origin: Option<Rect>, reveal: f32) -> Rect {
    /// How small the panel gets at the start of the motion.
    const MIN_SCALE: f32 = 0.86;

    let (w, h) = panel;
    let full = Rect::from_xywh(
        output.x() + (output.w() - w).max(0) / 2,
        output.y() + (output.h() - h).max(0) / 2,
        w,
        h,
    );
    let t = reveal.clamp(0.0, 1.0);
    if t >= 1.0 {
        return full;
    }

    let scale = MIN_SCALE + (1.0 - MIN_SCALE) * t;
    let (sw, sh) = ((w as f32 * scale) as i32, (h as f32 * scale) as i32);

    // The centre travels from the origin to the middle of the screen; with no
    // origin it simply grows in place.
    let from = origin.unwrap_or(full);
    let (fx, fy) = (
        from.x() + from.w() / 2,
        from.y() + from.h() / 2,
    );
    let (tx, ty) = (full.x() + full.w() / 2, full.y() + full.h() / 2);
    let cx = fx + ((tx - fx) as f32 * t) as i32;
    let cy = fy + ((ty - fy) as f32 * t) as i32;

    Rect::from_xywh(cx - sw / 2, cy - sh / 2, sw, sh)
}

/// Draw the launcher for `output` at `density` pixels per logical one.
pub(crate) fn render(
    launcher: &Launcher,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> Panel {
    Panel::from_canvas(&compose(launcher, apps, text, icons, pixmaps, output, density), density)
}

/// Lay the launcher out and paint it. Split from [`render`] so a test can get
/// at the pixels without going through a renderer.
fn compose(
    launcher: &Launcher,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> Canvas {
    // Everything here is in the canvas's own pixels, which `density` makes
    // more numerous than the logical ones the panel is placed in. See
    // `Panel::from_canvas`.
    let density = density.max(1);
    let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
    let size = BASE_SIZE * scale;
    let pad = PAD * scale;
    let width = (WIDTH * scale) as usize;
    let row = size * 2.2;
    let field = size * 2.6;

    // Only the rows that will be shown, so the panel is as tall as its content
    // rather than always reserving room for a full list.
    let shown = launcher.results().len().min(VISIBLE);
    let height = (pad * 2.0 + field + if shown > 0 { row * shown as f32 } else { 0.0 }) as usize;

    let mut canvas = Canvas::new(width, height.max(1));
    canvas.fill(0, 0, width, height, crate::theme::BACKGROUND.with_alpha(ALPHA).to_rgba_bytes());
    canvas.frame(crate::theme::BORDER.to_rgba_bytes());

    // The query, or a hint in the dim colour. A field that looks empty and a
    // field that looks like it contains the word "Search" are different
    // things, and the colour is what tells them apart.
    let (query_text, query_color) = if launcher.query().is_empty() {
        (PLACEHOLDER, crate::theme::TEXT_DIM)
    } else {
        (launcher.query(), crate::theme::TEXT)
    };
    let caret_w = 2.0 * scale;
    let text_y = (pad + (field - size * 1.6) / 2.0) as i32;
    // The placeholder starts clear of the caret. Both at `pad` puts a bar
    // through the first letter of the hint, which reads as a rendering fault
    // rather than as a cursor.
    let text_x = if launcher.query().is_empty() {
        pad + caret_w + 4.0 * scale
    } else {
        pad
    };
    text.draw(&mut canvas, query_text, size * 1.25, text_x as i32, text_y, query_color);

    // A caret, so the field reads as focused even when it is empty.
    // Measured from the query, not the placeholder, so an empty field puts the
    // caret at the start rather than after the hint text.
    let caret_x = pad + text.measure(launcher.query(), size * 1.25).0 + 2.0;
    canvas.fill(
        caret_x as usize,
        text_y as usize,
        caret_w as usize,
        (size * 1.4) as usize,
        crate::theme::ACCENT.to_rgba_bytes(),
    );

    canvas.fill(
        pad as usize,
        (pad + field) as usize,
        width - (pad * 2.0) as usize,
        1,
        crate::theme::BORDER.to_rgba_bytes(),
    );

    // The window onto the results: keep the selection on screen when it has
    // been moved past the bottom of a list longer than VISIBLE.
    let selected = launcher
        .results()
        .iter()
        .position(|i| Some(*i) == launcher.selection())
        .unwrap_or(0);
    let first = selected.saturating_sub(VISIBLE - 1);

    let mut y = pad + field;
    for (offset, index) in launcher.results().iter().skip(first).take(VISIBLE).enumerate() {
        let Some(entry) = apps.get(*index) else {
            continue;
        };
        let highlighted = first + offset == selected;
        if highlighted {
            canvas.tint(1, y as usize, width - 2, row as usize, crate::theme::ACCENT, 0x2E);
            // A solid bar down the left edge, so the highlight is legible even
            // where the wash behind it is subtle.
            canvas.fill(1, y as usize, (3.0 * scale) as usize, row as usize,
                        crate::theme::ACCENT.to_rgba_bytes());
        }
        // The icon, if the theme has one. An application with no icon gets
        // its name at the same indent as everything else rather than shifted
        // left, so the column of names stays a column.
        let icon_size = (size * 1.5) as u32;
        let icon_x = pad + 8.0 * scale;
        if let Some(pixmap) = entry
            .icon
            .as_deref()
            .and_then(|name| icons.find(name, icon_size / density, density))
            .and_then(|path| pixmaps.get(&path, icon_size))
        {
            canvas.blit(
                icon_x as usize,
                (y + (row - icon_size as f32) / 2.0) as usize,
                pixmap,
            );
        }
        text.draw(
            &mut canvas,
            &entry.name,
            size,
            (icon_x + icon_size as f32 + 10.0 * scale) as i32,
            (y + (row - size * 1.35) / 2.0) as i32,
            if highlighted { crate::theme::TEXT } else { crate::theme::TEXT_DIM },
        );
        y += row;
    }

    canvas
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use super::tests::{CLOCK, NOW, STILL, apps};

    fn drawn(query: &str, down: usize) -> (Canvas, Vec<Entry>) {
        let mut text = Text::new();
        let apps = apps();
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        for _ in 0..down {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let canvas = compose(
            &launcher,
            &apps,
            &mut text,
            &icons,
            &mut pixmaps,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        );
        (canvas, apps)
    }

    const OUTPUT: Rect = Rect::from_xywh(0, 0, 1920, 1080);
    const PANEL: (i32, i32) = (560, 360);
    /// A plausible dock launcher icon, bottom-centre.
    const ICON: Rect = Rect::from_xywh(840, 1010, 44, 44);

    #[test]
    fn fully_open_it_is_centred_at_full_size() {
        let rect = placement(OUTPUT, PANEL, Some(ICON), 1.0);
        assert_eq!((rect.w(), rect.h()), PANEL, "it did not reach full size");
        let left = rect.x();
        let right = OUTPUT.w() - (rect.x() + rect.w());
        assert!((left - right).abs() <= 1, "off centre by {}", left - right);
    }

    #[test]
    fn it_starts_at_the_dock_icon_and_travels_to_the_centre() {
        // §4: "fade + scale up from the dock icon's position".
        let start = placement(OUTPUT, PANEL, Some(ICON), 0.0);
        let icon_centre = (ICON.x() + ICON.w() / 2, ICON.y() + ICON.h() / 2);
        let start_centre = (start.x() + start.w() / 2, start.y() + start.h() / 2);
        assert_eq!(start_centre, icon_centre, "it did not start at the icon");

        // And moves monotonically toward the middle of the screen.
        let mut previous = start_centre.1;
        for step in 1..=10 {
            let rect = placement(OUTPUT, PANEL, Some(ICON), step as f32 / 10.0);
            let y = rect.y() + rect.h() / 2;
            assert!(y <= previous, "it moved back down at {step}");
            previous = y;
        }
    }

    #[test]
    fn it_scales_up_rather_than_appearing_at_full_size() {
        let small = placement(OUTPUT, PANEL, Some(ICON), 0.0);
        let big = placement(OUTPUT, PANEL, Some(ICON), 1.0);
        assert!(small.w() < big.w() && small.h() < big.h(), "it did not grow");
    }

    #[test]
    fn it_never_shrinks_to_an_unreadable_smear() {
        // Growing from the icon's literal 44 pixels means the first frames
        // are a blur of nothing. The motion carries the meaning; the content
        // still has to be legible while it happens.
        let start = placement(OUTPUT, PANEL, Some(ICON), 0.0);
        assert!(
            start.w() as f32 > PANEL.0 as f32 * 0.8,
            "it collapsed to {}x{}",
            start.w(),
            start.h()
        );
    }

    #[test]
    fn with_no_dock_it_grows_in_place_from_the_centre() {
        // A keyboard shortcut with the dock hidden: scaling up from off the
        // bottom of the screen is motion the eye cannot follow.
        let start = placement(OUTPUT, PANEL, None, 0.0);
        let full = placement(OUTPUT, PANEL, None, 1.0);
        let centre = |r: Rect| (r.x() + r.w() / 2, r.y() + r.h() / 2);
        assert_eq!(centre(start), centre(full), "it travelled with no origin");
        assert!(start.w() < full.w(), "it did not scale");
    }

    #[test]
    fn dismissing_reverses_the_same_motion() {
        // §4: "Dismiss reverses the same motion." Same reveal, same geometry —
        // which is the property that makes it reversible rather than a second
        // animation that happens to look similar.
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let opening = placement(OUTPUT, PANEL, Some(ICON), t);
            let closing = placement(OUTPUT, PANEL, Some(ICON), t);
            assert_eq!(opening, closing);
        }
    }

    #[test]
    fn the_query_survives_the_dismissal_animation() {
        // The panel is still on screen shrinking away; clearing the query as
        // it starts would show the placeholder for the last few frames.
        let apps = apps();
        let (mut launcher, _) = typed_with("firefox");
        assert!(!launcher.query().is_empty());
        launcher.close(CLOCK, STILL);
        assert!(!launcher.is_open());
        assert_eq!(launcher.query(), "firefox", "the query was cleared mid-dismissal");
        // Reopening still starts clean.
        launcher.open(&apps, &Frecency::new(), NOW, None, CLOCK, STILL);
        assert!(launcher.query().is_empty());
    }

    /// Open a launcher over `apps()` and type `query`.
    fn typed_with(query: &str) -> (Launcher, Vec<Entry>) {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        (launcher, apps)
    }

    #[test]
    fn the_panel_is_opaque_everywhere_including_under_the_highlight() {
        // Regression: the highlight was drawn with `fill` and a translucent
        // colour, which replaces alpha rather than mixing — so the selected
        // row was not tinted, it was a hole punched through the panel onto the
        // desktop. A wash has to be blended in, exactly as a glyph is.
        let (canvas, _) = drawn("", 1);
        let holes = canvas
            .pixels
            .chunks_exact(4)
            .filter(|p| p[3] < 0x80)
            .count();
        assert_eq!(holes, 0, "{holes} near-transparent pixels in the panel");
    }

    #[test]
    fn the_highlighted_row_is_tinted_rather_than_flooded() {
        // A solid accent row is unreadable and was what the bug produced.
        let (canvas, _) = drawn("", 0);
        let accent = crate::theme::ACCENT.to_rgba_bytes();
        let flooded = canvas
            .pixels
            .chunks_exact(4)
            .filter(|p| p[0] == accent[0] && p[1] == accent[1] && p[2] == accent[2])
            .count();
        // The caret and the left bar are solid accent; a flooded row would be
        // thousands of pixels.
        assert!(flooded < 2_000, "{flooded} solid-accent pixels; the row is flooded");
    }

    #[test]
    fn the_panel_grows_and_shrinks_with_the_result_count() {
        let (many, _) = drawn("", 0);
        let (few, _) = drawn("raven", 0);
        assert!(few.height < many.height, "the panel did not shrink to its results");
    }

    #[test]
    fn a_query_matching_nothing_still_draws_a_field() {
        // An empty result set must not produce a zero-height canvas.
        let (canvas, _) = drawn("qqzzxx", 0);
        assert!(canvas.height > 0 && canvas.stride > 0);
    }

    /// Dump the launcher to a PPM so it can be looked at.
    ///
    /// `LAUNCHER_DUMP=/tmp/l.ppm LAUNCHER_QUERY=fi cargo test -p huginn-comp launcher_dump`
    #[test]
    fn launcher_dump() {
        let Ok(path) = std::env::var("LAUNCHER_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let apps = scan_applications();
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, 0, None, CLOCK, STILL);
        for c in std::env::var("LAUNCHER_QUERY").unwrap_or_default().chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, 0, CLOCK, STILL);
        }
        for _ in 0..std::env::var("LAUNCHER_DOWN")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
        {
            launcher.press(Key::Down, &apps, &frecency, 0, CLOCK, STILL);
        }

        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        let canvas = compose(&launcher, &apps, &mut text, &icons, &mut pixmaps, output, 1);

        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.chunks_exact(4) {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }
}
