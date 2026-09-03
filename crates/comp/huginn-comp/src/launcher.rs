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

use raven_desktop::{Entry, FileIndex, Frecency, Icons, Pixmaps, calculate, entry, search};

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
    /// Move the selection. With nothing typed the suggestions are a grid,
    /// so `Up`/`Down` step a row and `Left`/`Right` a column; with a query
    /// the results are a list and `Left`/`Right` do nothing.
    Up,
    Down,
    Left,
    Right,
    /// Launch the selected application.
    Launch,
    /// Show, or hide, the selected application's other ways to start.
    Actions,
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
            keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => Self::Actions,
            keysyms::KEY_BackSpace if ctrl => Self::DeleteWord,
            keysyms::KEY_BackSpace => Self::Backspace,
            keysyms::KEY_Up => Self::Up,
            keysyms::KEY_Down => Self::Down,
            keysyms::KEY_Left => Self::Left,
            keysyms::KEY_Right => Self::Right,
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
    /// Close it and run `argv`. `entry` is the desktop file it was run on
    /// behalf of — which gets the credit in frecency, whichever of its
    /// actions ran — or `None` for a file being opened.
    Launch {
        entry: Option<std::path::PathBuf>,
        argv: Vec<String>,
    },
    /// Pin `entry` to the pinned panel, or unpin it if it already is. The
    /// launcher stays open: pinning is bookkeeping, not a launch, and the
    /// user may well want to pin two things in a row. The compositor owns
    /// the pin list (see [`crate::pins`]) and tells the launcher what it
    /// now holds through [`Launcher::set_pinned`].
    TogglePin { entry: std::path::PathBuf },
}

/// What a re-rank does with the highlight. Typing asks a new question and
/// gets the best answer on top; a list or index arriving underneath asks
/// nothing, and the highlight stays on what it was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    Top,
    Target,
}

/// Something the highlight can be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// An application, as an index into the application list.
    App(usize),
    /// A file, as an index into the file index.
    File(usize),
    /// The typed query, run as a shell command. Offered whenever nothing
    /// installed answers to it: what was typed may well be a program that
    /// has no desktop entry, and "no results" is a poorer answer than
    /// "run it, then".
    Command,
    /// The typed query, evaluated as arithmetic. Not something to launch —
    /// see [`Launcher::launch`] for what Enter does on it.
    Result,
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
    /// Where the drawn window of `results` starts. See [`Self::window`].
    first: usize,
    /// What can be highlighted, in navigation order: the suggested tiles and
    /// then the recent rows before anything is typed; every result and then
    /// the files after. `selected` indexes this. What is drawn is narrower:
    /// see [`Self::window`].
    visible: Vec<Target>,
    /// Files matching the query, best first, as indices into `files`.
    file_hits: Vec<usize>,
    /// The query evaluated as arithmetic, when it is arithmetic. See
    /// [`raven_desktop::calc`].
    result: Option<String>,
    /// The user's files, as last indexed. Shared with whoever builds it, and
    /// swapped whole: a search never sees a half-built index.
    files: std::sync::Arc<FileIndex>,
    /// Recently launched applications not already among the suggestions, most
    /// recent first, with when they were last launched (unix seconds).
    recent: Vec<(usize, u64)>,
    /// The clock the list was ranked against, for "2m ago".
    now: u64,
    /// The actions menu for the selection, if it is up: which item is
    /// highlighted, where 0 is "Open" and `n` is the entry's `n-1`th action.
    menu: Option<usize>,
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
    /// Where the last composition put everything the pointer can land on.
    /// Written back by the compositor after each redraw — the renderer is
    /// what knows where the rows ended up — and read by [`Self::hover`] and
    /// [`Self::click`], so the mouse and the keyboard agree on what is where.
    layout: Layout,
    /// The desktop files currently pinned, so the actions menu can offer
    /// "Pin" or "Unpin" without being handed the pin list on every key. Set
    /// by the compositor when the launcher opens and after each toggle.
    pinned: Vec<std::path::PathBuf>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            results: Vec::new(),
            first: 0,
            visible: Vec::new(),
            file_hits: Vec::new(),
            result: None,
            files: std::sync::Arc::default(),
            recent: Vec::new(),
            now: 0,
            menu: None,
            reveal: crate::anim::Animated::settled(0.0),
            origin: None,
            layout: Layout::default(),
            pinned: Vec::new(),
        }
    }
}

/// Wrap `argv` so it runs inside the desktop's terminal.
///
/// `Terminal=true` means the program is a TUI: spawned bare it has no
/// controlling terminal, so Vim or Htop exits at once and the launcher looks
/// like it did nothing. The terminal is the one the dock pins and the spawn
/// binding opens — [`crate::theme::TERMINAL`] — resolved through its own
/// desktop entry when one is installed, so a launcher wrapper in its `Exec=`
/// is honoured, and falling back to the bare binary name on `$PATH` when none
/// is. Pure, so the shape of the command line is testable without spawning.
///
/// The `-e` convention is assumed: RavenTerminal's source is not in this
/// workspace, and `-e <command...>` is what xterm, gnome-terminal, alacritty,
/// foot and kitty all accept, so it is the flag a terminal that wants to be a
/// drop-in for any of them ends up taking. If RavenTerminal ever spells it
/// differently, this is the one place to change.
pub(crate) fn in_terminal(argv: Vec<String>, entries: &[Entry]) -> Vec<String> {
    let mut wrapped = terminal_argv(entries);
    wrapped.push("-e".to_owned());
    wrapped.extend(argv);
    wrapped
}

/// The command line that opens the desktop's terminal.
///
/// Looked up by desktop-file stem, the same way `crate::dock` finds its pinned
/// entry, so the launcher and the dock agree on which terminal that is.
fn terminal_argv(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .find(|e| {
            e.path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case(crate::theme::TERMINAL))
        })
        .and_then(|e| e.argv(&[]))
        .unwrap_or_else(|| vec![crate::theme::TERMINAL.to_owned()])
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
    /// `None` when nothing is, or when the highlight is on a file.
    pub(crate) fn selection(&self) -> Option<usize> {
        match self.target() {
            Some(Target::App(i)) => Some(i),
            _ => None,
        }
    }

    /// What is highlighted, application or file.
    pub(crate) fn target(&self) -> Option<Target> {
        self.visible.get(self.selected).copied()
    }

    /// The application rows the list draws: at most [`VISIBLE`] of
    /// [`Self::results`], starting at `first`.
    ///
    /// `first` is scroll state, kept rather than derived from the selection,
    /// because the keyboard and the mouse want different things of it. The
    /// highlight walking down past the last drawn row pulls the window down
    /// one row at a time, and walking back up past the first pulls it back
    /// (see [`Self::scroll_to_selection`]). The pointer, on the other hand,
    /// only ever lands on a row that is already drawn — and a window that
    /// re-derived itself around the hovered row would slide the rows out
    /// from under the pointer, so the next motion event highlighted a
    /// different application than the one the hand is over. With the
    /// highlight below the applications — on the files or the run row — the
    /// window stays where it was when the highlight left.
    pub(crate) fn window(&self) -> &[usize] {
        let len = self.results.len();
        let first = self.first.min(len.saturating_sub(VISIBLE));
        &self.results[first..(first + VISIBLE).min(len)]
    }

    /// Slide the window the least that puts the highlight among the drawn
    /// rows. Called after the keyboard moves the highlight and after the
    /// results change; deliberately not after a hover, see [`Self::window`].
    fn scroll_to_selection(&mut self) {
        let last_window = self.results.len().saturating_sub(VISIBLE);
        if let (Some(Target::App(_)), false) = (self.target(), self.is_grid()) {
            // The application section of `visible` mirrors `results`,
            // offset by the result row when there is one.
            let position = self.selected - usize::from(self.result.is_some());
            self.first = self
                .first
                .clamp(position.saturating_sub(VISIBLE - 1), position);
        }
        self.first = self.first.min(last_window);
    }

    /// Files matching the query, best first. Empty in the grid.
    pub(crate) fn file_hits(&self) -> &[usize] {
        &self.file_hits
    }

    /// The index the file rows are drawn from.
    pub(crate) fn files(&self) -> &FileIndex {
        &self.files
    }

    /// The query's value as arithmetic, if it is arithmetic: the text of the
    /// result row, without its "=".
    pub(crate) fn result(&self) -> Option<&str> {
        self.result.as_deref()
    }

    /// Whether a "Run" row is offered for the query.
    ///
    /// Only when no application answered: an application match is what the
    /// user almost certainly meant, and a second row offering to run its
    /// name through the shell would be a trap one keystroke below it. Files
    /// do not suppress it — a file called "make" is not what "make" meant.
    pub(crate) fn offers_command(&self) -> bool {
        !self.query.is_empty() && self.results.is_empty()
    }

    /// Use a newly built index. The next re-rank searches it.
    pub(crate) fn set_files(&mut self, files: std::sync::Arc<FileIndex>) {
        self.files = files;
    }

    /// Recently launched applications, most recent first, each with when.
    /// Empty unless the grid is showing.
    pub(crate) fn recent(&self) -> &[(usize, u64)] {
        &self.recent
    }

    /// The clock the panel was ranked against, in unix seconds.
    pub(crate) fn now(&self) -> u64 {
        self.now
    }

    /// The highlighted item of the actions menu, if the menu is up. See
    /// [`Self::menu_items`] for what the number means.
    pub(crate) fn menu(&self) -> Option<usize> {
        self.menu
    }

    /// What the actions menu offers for `entry`: "Open", each action, and
    /// last — where a stray Down cannot land on it by accident — "Pin", or
    /// "Unpin" when it already is.
    pub(crate) fn menu_items<'a>(&self, entry: &'a Entry) -> Vec<&'a str> {
        std::iter::once("Open")
            .chain(entry.actions.iter().map(|a| a.name.as_str()))
            .chain(std::iter::once(if self.is_pinned(entry) {
                UNPIN
            } else {
                PIN
            }))
            .collect()
    }

    /// Whether `entry` is on the pinned panel, as last told.
    pub(crate) fn is_pinned(&self, entry: &Entry) -> bool {
        self.pinned.contains(&entry.path)
    }

    /// Tell the launcher what is pinned. The menu, if it is up, relabels
    /// itself on the next redraw.
    pub(crate) fn set_pinned(&mut self, pinned: Vec<std::path::PathBuf>) {
        self.pinned = pinned;
    }

    /// Whether the panel is showing the suggestion grid rather than a list.
    ///
    /// Nothing typed means no question asked, and the honest answer to no
    /// question is "here is what you usually want" — a handful of tiles,
    /// not a ranked list of everything installed.
    pub(crate) fn is_grid(&self) -> bool {
        self.query.is_empty()
    }

    /// How many suggestion tiles are showing.
    fn tiles(&self) -> usize {
        if self.is_grid() {
            self.results.len().min(SUGGESTED)
        } else {
            0
        }
    }

    /// Whether the highlight is on a suggestion tile rather than a row.
    fn on_tile(&self) -> bool {
        self.selected < self.tiles()
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
        self.menu = None;
        self.origin = origin;
        self.refresh(entries, frecency, now, Keep::Top);
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
        self.menu = None;
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

    /// Re-aim the motion at where the dock icon is *now*.
    ///
    /// The origin is a global rect captured at open; an output relayout or a
    /// focus change moves the dock out from under it, and a close animation
    /// shrinking toward the old rect sweeps the panel across the wrong screen.
    pub(crate) fn set_origin(&mut self, origin: Option<Rect>) {
        self.origin = origin;
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
        // The menu, while it is up, takes the keys that mean something to
        // it. Anything else — a character, a deletion — is about the query,
        // and the menu was about a selection that is about to change.
        if let Some(item) = self.menu {
            match key {
                Key::Dismiss => {
                    self.menu = None;
                    return Outcome::Redraw;
                }
                Key::Actions => {
                    self.menu = None;
                    return Outcome::Redraw;
                }
                Key::Up | Key::Down => {
                    let count = self
                        .selection()
                        .and_then(|i| entries.get(i))
                        .map_or(1, |e| self.menu_items(e).len());
                    let next = if key == Key::Up {
                        item.saturating_sub(1)
                    } else {
                        (item + 1).min(count - 1)
                    };
                    if next == item {
                        return Outcome::Unchanged;
                    }
                    self.menu = Some(next);
                    return Outcome::Redraw;
                }
                Key::Left | Key::Right | Key::Ignored => return Outcome::Unchanged,
                Key::Launch => return self.launch(entries, clock, motion),
                Key::Insert(_) | Key::Backspace | Key::DeleteWord | Key::Clear => {
                    self.menu = None;
                }
            }
        }
        match key {
            Key::Dismiss => {
                self.close(clock, motion);
                Outcome::Dismissed
            }
            Key::Launch => self.launch(entries, clock, motion),
            Key::Actions => {
                // Nothing selected, nothing to act on: a menu for no entry
                // would be a heading with no items.
                if self.selection().is_none() {
                    return Outcome::Unchanged;
                }
                self.menu = Some(0);
                Outcome::Redraw
            }
            Key::Up => self.move_vertically(-1),
            Key::Down => self.move_vertically(1),
            // A row has no sideways; the keys are swallowed rather than
            // forwarded, like every other key while the launcher is open.
            Key::Left if !self.on_tile() => Outcome::Unchanged,
            Key::Right if !self.on_tile() => Outcome::Unchanged,
            Key::Left => self.move_selection(-1),
            Key::Right => self.move_selection(1),
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

    /// Run the selection — or, with the menu up, the highlighted item of it.
    fn launch(
        &mut self,
        entries: &[Entry],
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        // A file opens with whatever handles its type. `xdg-open` is the
        // one door every desktop agrees on; resolving the MIME type and the
        // handler ourselves would be a second, disagreeing implementation.
        match self.target() {
            Some(Target::File(i)) => {
                let Some(file) = self.files.get(i) else {
                    return Outcome::Unchanged;
                };
                let argv = vec![
                    "xdg-open".to_owned(),
                    file.path.to_string_lossy().into_owned(),
                ];
                self.close(clock, motion);
                return Outcome::Launch { entry: None, argv };
            }
            // The query, through the shell, so "ls -la ~" means what it
            // means at a prompt — the row said "Run", and a shell is what
            // runs things. No entry to credit: there is no desktop file.
            Some(Target::Command) => {
                let argv = vec!["sh".to_owned(), "-c".to_owned(), self.query.clone()];
                self.close(clock, motion);
                return Outcome::Launch { entry: None, argv };
            }
            // A result is an answer, not an action. The natural thing for
            // Enter to do would be to copy it, but the launcher has no
            // clipboard: it is drawn by the compositor, which has no
            // selection of its own to set. So Enter does nothing — the
            // value stays on screen to be read, and the launcher stays open
            // rather than closing on a key that did nothing.
            Some(Target::Result) => return Outcome::Unchanged,
            Some(Target::App(_)) | None => {}
        }
        // Return on an empty result set does nothing rather than closing, so
        // a typo does not dismiss what you were typing.
        let Some(entry) = self.selection().and_then(|i| entries.get(i)) else {
            return Outcome::Unchanged;
        };
        let argv = match self.menu {
            None | Some(0) => entry.argv(&[]),
            // The last item pins or unpins rather than running anything.
            Some(n) if n + 1 == self.menu_items(entry).len() => {
                self.menu = None;
                return Outcome::TogglePin {
                    entry: entry.path.clone(),
                };
            }
            Some(n) => entry
                .actions
                .get(n - 1)
                .and_then(|action| entry.action_argv(action, &[])),
        };
        match argv {
            Some(argv) => {
                // An action inherits the entry's `Terminal=`: the spec gives
                // actions no key of their own, and an action of a TUI is
                // still a TUI.
                let argv = if entry.terminal {
                    in_terminal(argv, entries)
                } else {
                    argv
                };
                let entry = Some(entry.path.clone());
                self.close(clock, motion);
                Outcome::Launch { entry, argv }
            }
            // An Exec that resolves to nothing must not close the launcher
            // — silently doing nothing would look like the key was ignored.
            None => Outcome::Unchanged,
        }
    }

    /// Where the last redraw put things. See [`Layout`].
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Remember where the redraw put things, so the pointer can find them.
    pub(crate) fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
    }

    /// The pointer moved to `point`, in canvas pixels, over the panel.
    ///
    /// The highlight follows the pointer, the way it follows the arrow keys:
    /// one highlight, moved by whichever the hand is on. With the menu up
    /// the pointer moves the menu's own highlight instead and leaves the
    /// selection alone — the menu is about that selection, and sliding it
    /// out from under the menu would leave a menu offering one application's
    /// actions under another's name. Over nothing — a heading, the field,
    /// the footer — the highlight stays where it was rather than vanishing.
    pub(crate) fn hover(&mut self, point: huginn_core::geometry::Point) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        if self.menu.is_some() {
            return match self.layout.menu_hit(point) {
                Some(item) if self.menu != Some(item) => {
                    self.menu = Some(item);
                    Outcome::Redraw
                }
                _ => Outcome::Unchanged,
            };
        }
        match self.layout.hit(point) {
            Some(index) if index < self.visible.len() && index != self.selected => {
                self.selected = index;
                Outcome::Redraw
            }
            _ => Outcome::Unchanged,
        }
    }

    /// A click at `point`, in canvas pixels, over the panel.
    ///
    /// A click on a target is a hover and then Enter: the highlight moves
    /// there and it launches, through the same path the key takes, so a
    /// mouse launch is credited to frecency exactly as a keyboard one is.
    /// With the menu up, a click on one of its items runs that item, and a
    /// click anywhere else on the panel puts the menu away — the same thing
    /// Escape does — rather than launching what was under the menu's edge.
    /// A click on nothing is nothing.
    pub(crate) fn click(
        &mut self,
        point: huginn_core::geometry::Point,
        entries: &[Entry],
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        if self.menu.is_some() {
            return match self.layout.menu_hit(point) {
                Some(item) => {
                    self.menu = Some(item);
                    self.launch(entries, clock, motion)
                }
                None => {
                    self.menu = None;
                    Outcome::Redraw
                }
            };
        }
        let moved = self.hover(point);
        if self.layout.hit(point).is_none() {
            return moved;
        }
        match self.launch(entries, clock, motion) {
            // A click on a row that cannot launch — the result row, an
            // entry whose Exec resolves to nothing — still moved the
            // highlight, and the highlight has to be drawn where it went.
            Outcome::Unchanged => moved,
            outcome => outcome,
        }
    }

    /// Re-rank after the query changed, and put the selection back on top.
    ///
    /// Resetting to the first result is the whole point of a search-first
    /// launcher: every keystroke is a new question, and the answer is the best
    /// match, not whatever happened to be highlighted for the previous query.
    fn after_edit(&mut self, entries: &[Entry], frecency: &Frecency, now: u64) -> Outcome {
        self.selected = 0;
        self.refresh(entries, frecency, now, Keep::Top);
        Outcome::Redraw
    }

    /// Re-rank against a changed application list, keeping the query.
    ///
    /// Results are indices into that list, so a list that changed underneath
    /// an open launcher leaves the highlight pointing at whatever now sits at
    /// that index — the wrong application, and it would be the one Enter
    /// launches. Re-ranking is what keeps the indices meaning what they say.
    ///
    /// The highlight stays on what it was on. A file index arriving in the
    /// background is not a keystroke: the user asked nothing new, so the
    /// Run row they were about to press Enter on must not turn into a
    /// freshly found file, and the actions menu they had up must not close.
    pub(crate) fn reindex(&mut self, entries: &[Entry], frecency: &Frecency, now: u64) {
        self.refresh(entries, frecency, now, Keep::Target);
    }

    fn refresh(&mut self, entries: &[Entry], frecency: &Frecency, now: u64, keep: Keep) {
        self.now = now;
        let old = self.target();
        self.results = search(entries, &self.query, frecency, now)
            .into_iter()
            .map(|hit| hit.index)
            .collect();
        self.recent.clear();
        if self.is_grid() {
            let tiles = self.results.len().min(SUGGESTED);
            // Not already a tile: a row that repeats a tile a few pixels up
            // tells the user nothing they were not just looking at.
            let shown = &self.results[..tiles];
            self.recent = entries
                .iter()
                .enumerate()
                .filter(|(i, _)| !shown.contains(i))
                .filter_map(|(i, e)| frecency.last_used(&e.path).map(|t| (i, t)))
                .collect();
            self.recent
                .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            self.recent.truncate(RECENT);
            self.file_hits.clear();
            self.result = None;
            self.visible = shown
                .iter()
                .chain(self.recent.iter().map(|(i, _)| i))
                .map(|i| Target::App(*i))
                .collect();
        } else {
            // Every application is navigable, not only the rows that fit:
            // the list scrolls under the highlight (see [`Self::window`]),
            // so the ninth match is one more Down away rather than
            // unreachable. The files come after the last application,
            // however many there were. Not before the last term is two
            // characters long, though: one letter matches most of a large
            // index, and this runs here, on the compositor thread, on every
            // keystroke — so `f` lists Firefox and Files, and no files.
            self.file_hits = self.files.search(&self.query, FILES);
            // The arithmetic result first: a query that evaluates was asked
            // for its value, and a value is read, not chosen. The command
            // last: it is the fallback, offered after everything the desktop
            // could find, and the highlight should land on it only when
            // there was nothing else to land on.
            self.result = calculate(&self.query);
            self.visible = self
                .result
                .iter()
                .map(|_| Target::Result)
                .chain(self.results.iter().map(|i| Target::App(*i)))
                .chain(self.file_hits.iter().map(|i| Target::File(*i)))
                .chain(self.offers_command().then_some(Target::Command))
                .collect();
        }
        // Files and the command row shift in navigation order whenever an
        // index arrives with more or fewer hits above them, so they are
        // re-found by what they are. An application is re-found by where it
        // was: its index is into a list that may just have been reshuffled
        // by an install, and the same rank in the same ranking is the same
        // application (see `reindexing_keeps_the_highlight_on_the_application_it_was_on`).
        let found = match (keep, old) {
            (Keep::Target, Some(Target::App(_))) => Some(self.selected),
            (Keep::Target, Some(old)) => self.visible.iter().position(|t| *t == old),
            _ => None,
        };
        // Results can shrink under a selection that was valid a keystroke ago.
        // Clamping here rather than at every read is what stops `selection`
        // pointing past the end and the highlight vanishing.
        self.selected = found
            .unwrap_or(self.selected)
            .min(self.visible.len().saturating_sub(1));
        // A menu that outlived the entry it was for would offer one
        // application's actions under another's name; one whose entry is
        // still right there under it is left up.
        if !matches!(old, Some(Target::App(_))) || self.target() != old {
            self.menu = None;
        }
        self.scroll_to_selection();
    }

    /// `Up`/`Down`: a row of tiles at a time on the grid, a row at a time in
    /// a list, and from the bottom of the grid down into the recent rows.
    fn move_vertically(&mut self, direction: isize) -> Outcome {
        let tiles = self.tiles();
        if !self.on_tile() {
            // In a list, or in the recent rows. Up off the top of the recent
            // rows lands on the last tile.
            return self.move_selection(direction);
        }
        let next = self.selected as isize + direction * COLUMNS as isize;
        if next < 0 {
            return self.move_selection(-(self.selected as isize));
        }
        if next as usize >= tiles {
            // Past the last row of tiles: onto the first recent row if there
            // is one, otherwise stay on the last tile.
            let target = if self.visible.len() > tiles {
                tiles
            } else {
                tiles - 1
            };
            return self.move_selection(target as isize - self.selected as isize);
        }
        self.move_selection(next - self.selected as isize)
    }

    /// Move the highlight, stopping at the ends rather than wrapping.
    ///
    /// Not wrapping is deliberate: the list is ordered best-first, so falling
    /// off the bottom onto the best match again would move the highlight the
    /// furthest possible distance for one keypress.
    fn move_selection(&mut self, delta: isize) -> Outcome {
        if self.visible.is_empty() {
            return Outcome::Unchanged;
        }
        let last = self.visible.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        if next == self.selected {
            return Outcome::Unchanged;
        }
        self.selected = next;
        self.scroll_to_selection();
        Outcome::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(super) fn entry(name: &str, exec: &str) -> Entry {
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
            actions: Vec::new(),
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
        assert_eq!(
            selected_name(&launcher, &apps).as_deref(),
            Some("Raven Terminal")
        );
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
        assert!(
            launcher.results().len() < apps.len(),
            "the list did not narrow"
        );
        assert!(
            launcher.selection().is_some(),
            "the highlight fell off the list"
        );
    }

    #[test]
    fn the_selection_stops_at_the_ends_rather_than_wrapping() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);

        assert_eq!(
            launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(launcher.selection(), launcher.results().first().copied());

        for _ in 0..20 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.selection(), launcher.results().last().copied());
        assert_eq!(
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn before_typing_the_arrows_walk_a_grid() {
        // Four suggestions in three columns: Right steps one, Down steps a
        // row, and neither leaves the grid.
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert!(launcher.is_grid());
        let order: Vec<usize> = launcher.results().to_vec();

        launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(order[1]));
        launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(order[0]));
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(order[COLUMNS]));
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(order[0]));
    }

    #[test]
    fn typing_turns_the_grid_into_a_list() {
        let (mut launcher, apps) = typed("f");
        assert!(!launcher.is_grid());
        let frecency = Frecency::new();
        // Left and Right have nowhere to go in a list.
        assert_eq!(
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        let first = launcher.selection();
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_ne!(launcher.selection(), first, "Down did not step one row");
    }

    #[test]
    fn the_grid_never_selects_past_what_it_shows() {
        // More applications than tiles: the highlight stops at the last
        // tile rather than wandering onto a suggestion that is not drawn.
        let apps: Vec<Entry> = (0..10)
            .map(|i| entry(&format!("App {i}"), "/bin/app"))
            .collect();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for _ in 0..20 {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        }
        let last = launcher.results()[SUGGESTED - 1];
        assert_eq!(launcher.selection(), Some(last));
    }

    #[test]
    fn recently_launched_applications_are_listed_under_the_grid() {
        // Six applications used often fill the grid. A seventh, launched once
        // two minutes ago, scores nowhere near them — and is exactly what the
        // user most likely wants back.
        let apps: Vec<Entry> = (0..10)
            .map(|i| entry(&format!("App {i}"), "/bin/app"))
            .collect();
        let mut frecency = Frecency::new();
        for app in apps.iter().take(SUGGESTED) {
            for _ in 0..5 {
                frecency.record(&app.path, NOW - 3_600);
            }
        }
        frecency.record(&apps[9].path, NOW - 120);
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);

        let mut tiles: Vec<usize> = launcher.results()[..SUGGESTED].to_vec();
        tiles.sort_unstable();
        assert_eq!(tiles, (0..SUGGESTED).collect::<Vec<_>>());
        let recent: Vec<usize> = launcher.recent().iter().map(|(i, _)| *i).collect();
        assert_eq!(
            recent,
            vec![9],
            "a tile was repeated, or the recent one lost"
        );
        assert_eq!(launcher.recent()[0].1, NOW - 120);
    }

    #[test]
    fn down_from_the_last_tile_row_lands_on_the_recent_rows() {
        let apps: Vec<Entry> = (0..10)
            .map(|i| entry(&format!("App {i}"), "/bin/app"))
            .collect();
        // Seven launched, six tiles: one overflows into the recent rows.
        let mut frecency = Frecency::new();
        for (i, app) in apps.iter().take(7).enumerate() {
            frecency.record(&app.path, NOW - 1_000 + i as u64);
        }
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert!(!launcher.recent().is_empty(), "nothing overflowed the grid");
        let first_recent = launcher.recent()[0].0;

        // Down twice from the top-left tile: row two, then the recent rows.
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(launcher.results()[COLUMNS]));
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(first_recent));
        // Sideways does nothing on a row.
        assert_eq!(
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        // And Up goes back onto the grid.
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.selection(),
            Some(launcher.results()[SUGGESTED - 1])
        );
    }

    #[test]
    fn a_recent_row_launches_like_a_tile() {
        let apps: Vec<Entry> = (0..8)
            .map(|i| entry(&format!("App {i}"), &format!("/bin/app{i}")))
            .collect();
        let mut frecency = Frecency::new();
        for (i, app) in apps.iter().take(7).enumerate() {
            frecency.record(&app.path, NOW - 1_000 + i as u64);
        }
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        let target = launcher.recent()[0].0;
        for _ in 0..3 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.selection(), Some(target));
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: Some(apps[target].path.clone()),
                argv: vec![format!("/bin/app{target}")]
            }
        );
    }

    /// A browser with two actions, alone, so it is always the selection.
    fn browser() -> Vec<Entry> {
        let mut e = entry("Browser", "/bin/browser %U");
        e.actions = vec![
            raven_desktop::entry::Action {
                id: "new".into(),
                name: "New Window".into(),
                exec: "/bin/browser --new-window".into(),
                icon: None,
            },
            raven_desktop::entry::Action {
                id: "incognito".into(),
                name: "New Incognito Window".into(),
                exec: "/bin/browser --incognito %U".into(),
                icon: None,
            },
        ];
        vec![e]
    }

    #[test]
    fn tab_offers_open_and_the_entrys_actions() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert_eq!(launcher.menu(), None);
        assert_eq!(
            launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert_eq!(launcher.menu(), Some(0));
        assert_eq!(
            launcher.menu_items(&apps[0]),
            ["Open", "New Window", "New Incognito Window", "Pin"]
        );
        // Tab again puts it away.
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.menu(), None);
    }

    #[test]
    fn the_last_menu_item_pins_and_then_unpins_without_closing() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        for _ in 0..3 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::TogglePin {
                entry: apps[0].path.clone()
            }
        );
        assert!(launcher.is_open(), "pinning closed the launcher");
        assert_eq!(launcher.menu(), None, "the menu stayed up after pinning");
        // The compositor did the pinning, and says so; the label follows.
        launcher.set_pinned(vec![apps[0].path.clone()]);
        assert_eq!(launcher.menu_items(&apps[0]).last(), Some(&UNPIN));
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        for _ in 0..3 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert!(matches!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::TogglePin { .. }
        ));
    }

    #[test]
    fn enter_on_an_action_runs_that_action_for_the_same_entry() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        // Past the actions is "Pin", and the menu stops there rather than
        // wrapping.
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: Some(apps[0].path.clone()),
                argv: vec!["/bin/browser".to_owned(), "--incognito".to_owned()]
            }
        );
        assert!(!launcher.is_open());
    }

    #[test]
    fn open_at_the_top_of_the_menu_is_the_plain_launch() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: Some(apps[0].path.clone()),
                argv: vec!["/bin/browser".to_owned()]
            }
        );
    }

    #[test]
    fn escape_with_the_menu_up_closes_the_menu_not_the_launcher() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert!(launcher.is_open());
        assert_eq!(launcher.menu(), None);
        // A second Escape is the usual one.
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Dismissed
        );
    }

    #[test]
    fn typing_puts_the_menu_away_and_edits_the_query() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Insert('b'), &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.menu(), None);
        assert_eq!(launcher.query(), "b");
    }

    #[test]
    fn tab_with_nothing_selected_does_nothing() {
        let (mut launcher, apps) = typed("qqzzxx");
        assert!(launcher.selection().is_none());
        assert_eq!(
            launcher.press(Key::Actions, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(launcher.menu(), None);
    }

    #[test]
    fn tab_is_the_actions_key() {
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Tab, false, Some('\t')),
            Key::Actions
        );
    }

    /// A launcher over `apps()` that also knows about a few files.
    fn with_files(query: &str) -> (Launcher, Vec<Entry>) {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.set_files(std::sync::Arc::new(FileIndex::from_paths(
            std::path::Path::new("/home/u"),
            ["notes.md", "Documents/firefox-bookmarks.html", "photo.jpg"]
                .into_iter()
                .map(|p| PathBuf::from(format!("/home/u/{p}"))),
        )));
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        (launcher, apps)
    }

    #[test]
    fn files_are_listed_after_the_applications() {
        let (launcher, _) = with_files("fi");
        // Firefox and Files match as applications; the bookmarks file
        // matches as a file. Files come after, never instead.
        assert!(launcher.results().len() >= 2);
        assert_eq!(launcher.file_hits().len(), 1);
        assert_eq!(
            launcher.selection(),
            launcher.results().first().copied(),
            "an application must stay the first thing Enter runs"
        );
    }

    #[test]
    fn nothing_typed_lists_no_files() {
        let (launcher, _) = with_files("");
        assert!(launcher.is_grid());
        assert!(launcher.file_hits().is_empty());
    }

    #[test]
    fn a_file_opens_with_xdg_open_and_credits_no_application() {
        let (mut launcher, apps) = with_files("notes");
        let frecency = Frecency::new();
        assert!(
            launcher.results().is_empty(),
            "no application is called notes"
        );
        assert_eq!(launcher.selection(), None, "a file is not an application");
        assert!(matches!(launcher.target(), Some(Target::File(_))));
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: None,
                argv: vec!["xdg-open".to_owned(), "/home/u/notes.md".to_owned()]
            }
        );
        assert!(!launcher.is_open());
    }

    #[test]
    fn down_walks_from_the_last_application_onto_the_files() {
        let (mut launcher, apps) = with_files("fi");
        let frecency = Frecency::new();
        let rows = launcher.results().len();
        for _ in 0..rows {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert!(matches!(launcher.target(), Some(Target::File(_))));
        // A file has no actions menu.
        assert_eq!(
            launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(launcher.menu(), None);
    }

    /// Twelve applications answering to "tool": more than a screenful.
    fn many_tools() -> Vec<Entry> {
        (1..=12)
            .map(|n| entry(&format!("Tool {n:02}"), &format!("/bin/tool{n}")))
            .collect()
    }

    /// Open over `many_tools()` with the file index from `with_files`, and
    /// type "tool" — which the file `tool-notes.md` answers to as well.
    fn with_many_tools() -> (Launcher, Vec<Entry>) {
        let apps = many_tools();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.set_files(std::sync::Arc::new(FileIndex::from_paths(
            std::path::Path::new("/home/u"),
            [PathBuf::from("/home/u/tool-notes.md")],
        )));
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in "tool".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.results().len(), 12);
        assert_eq!(launcher.file_hits().len(), 1);
        (launcher, apps)
    }

    #[test]
    fn down_walks_every_application_before_the_files() {
        let (mut launcher, apps) = with_many_tools();
        let frecency = Frecency::new();
        for _ in 0..11 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        let last = *launcher.results().last().unwrap();
        assert_eq!(launcher.target(), Some(Target::App(last)));
        assert!(
            launcher.window().contains(&last),
            "the highlighted application is not drawn"
        );
        assert_eq!(launcher.window().len(), VISIBLE);
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert!(matches!(launcher.target(), Some(Target::File(_))));
        // The applications keep their last window under the files.
        assert_eq!(launcher.window(), &launcher.results()[12 - VISIBLE..]);
    }

    #[test]
    fn the_window_follows_the_highlight_both_ways() {
        let (mut launcher, apps) = with_many_tools();
        let frecency = Frecency::new();
        assert_eq!(launcher.window(), &launcher.results()[..VISIBLE]);
        for _ in 0..VISIBLE {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        // One past the first screenful: the window has slid one row.
        assert_eq!(launcher.window(), &launcher.results()[1..=VISIBLE]);
        for _ in 0..VISIBLE {
            launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.window(), &launcher.results()[..VISIBLE]);
        assert_eq!(launcher.selection(), launcher.results().first().copied());
    }

    #[test]
    fn a_short_list_is_its_own_window() {
        let (launcher, _) = typed("fi");
        assert!(launcher.results().len() <= VISIBLE);
        assert_eq!(launcher.window(), launcher.results());
    }

    #[test]
    fn a_new_index_is_searched_on_the_next_rerank() {
        let (mut launcher, apps) = with_files("photo");
        let frecency = Frecency::new();
        assert_eq!(launcher.file_hits().len(), 1);
        launcher.set_files(std::sync::Arc::new(FileIndex::default()));
        launcher.reindex(&apps, &frecency, NOW);
        assert!(launcher.file_hits().is_empty());
        // With the file gone nothing matches, and the fallback is all that
        // is left to highlight.
        assert_eq!(launcher.target(), Some(Target::Command));
    }

    #[test]
    fn timestamps_read_the_way_people_say_them() {
        assert_eq!(ago(0), "Just now");
        assert_eq!(ago(59), "Just now");
        assert_eq!(ago(120), "2m ago");
        assert_eq!(ago(7_200), "2h ago");
        assert_eq!(ago(3 * 86_400), "3d ago");
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
        assert!(
            launcher.query().is_empty(),
            "the query outlived the launcher"
        );
    }

    #[test]
    fn return_launches_the_selection_and_closes() {
        let (mut launcher, apps) = typed("fire");
        let outcome = launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(
            outcome,
            Outcome::Launch {
                entry: Some(apps[0].path.clone()),
                argv: vec!["/bin/firefox".to_owned()]
            }
        );
        assert!(!launcher.is_open());
    }

    #[test]
    fn launching_strips_field_codes() {
        // The argv comes from Entry::argv, so `%F` with nothing to open must
        // not reach the command line as a literal argument.
        let (mut launcher, apps) = typed("raven");
        let outcome = launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(
            outcome,
            Outcome::Launch {
                entry: Some(apps[3].path.clone()),
                argv: vec!["/bin/raven-terminal".to_owned()]
            }
        );
    }

    /// A `Terminal=true` entry with one action, over an installed terminal
    /// whose `Exec=` is a wrapper script rather than the bare binary.
    fn tui() -> Vec<Entry> {
        let mut htop = entry("Htop", "/bin/htop %U");
        htop.terminal = true;
        htop.actions.push(raven_desktop::entry::Action {
            id: "tree".to_owned(),
            name: "Tree view".to_owned(),
            exec: "/bin/htop --tree".to_owned(),
            icon: None,
        });
        let mut terminal = entry("Raven Terminal", "/usr/local/bin/raven-terminal-launcher");
        terminal.path = PathBuf::from("/apps/raven-terminal.desktop");
        vec![htop, terminal]
    }

    #[test]
    fn a_terminal_entry_is_wrapped_in_the_pinned_terminal() {
        let apps = tui();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in "htop".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: Some(apps[0].path.clone()),
                argv: vec![
                    "/usr/local/bin/raven-terminal-launcher".to_owned(),
                    "-e".to_owned(),
                    "/bin/htop".to_owned()
                ]
            }
        );
    }

    #[test]
    fn an_action_of_a_terminal_entry_is_wrapped_too() {
        let apps = tui();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in "htop".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Launch, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: Some(apps[0].path.clone()),
                argv: vec![
                    "/usr/local/bin/raven-terminal-launcher".to_owned(),
                    "-e".to_owned(),
                    "/bin/htop".to_owned(),
                    "--tree".to_owned()
                ]
            }
        );
    }

    #[test]
    fn a_graphical_entry_is_not_wrapped() {
        // `apps()` holds no `Terminal=true` entry, so Firefox runs bare even
        // with a terminal installed alongside it.
        let (mut launcher, apps) = typed("fire");
        assert_eq!(
            launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: Some(apps[0].path.clone()),
                argv: vec!["/bin/firefox".to_owned()]
            }
        );
    }

    #[test]
    fn the_wrapper_falls_back_to_the_bare_binary_without_a_terminal_entry() {
        // No desktop file with the terminal's stem: `apps()` names its
        // terminal "Raven Terminal.desktop", which is not it.
        assert_eq!(
            in_terminal(vec!["/bin/vim".to_owned()], &apps()),
            ["raven-terminal", "-e", "/bin/vim"]
        );
    }

    #[test]
    fn return_with_nothing_at_all_to_run_does_not_close_the_launcher() {
        // With no application, no file and no run row under the highlight
        // there is nothing for Enter to do, and doing nothing must not
        // dismiss what was being typed. The run row makes this rare — it
        // is offered for any non-empty query with no application — so the
        // case is reached by highlighting the result row, which Enter
        // deliberately does nothing on.
        let (mut launcher, apps) = typed("2+2");
        assert!(launcher.results().is_empty());
        assert_eq!(launcher.target(), Some(Target::Result));
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
            assert_eq!(
                launcher.press(key, &apps, &frecency, NOW, CLOCK, STILL),
                Outcome::Unchanged
            );
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

    // ---- the run row and the calculator ----

    #[test]
    fn a_query_matching_no_application_offers_to_run_it() {
        let (mut launcher, apps) = typed("htop -d 5");
        assert!(launcher.results().is_empty());
        assert!(launcher.offers_command());
        assert_eq!(launcher.target(), Some(Target::Command));
        assert_eq!(
            launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Launch {
                entry: None,
                argv: vec!["sh".to_owned(), "-c".to_owned(), "htop -d 5".to_owned()]
            }
        );
        assert!(!launcher.is_open());
    }

    #[test]
    fn an_application_match_suppresses_the_run_row() {
        // "fire" is Firefox. A row one step below it offering to run
        // `fire` through the shell would be a trap.
        let (launcher, _) = typed("fire");
        assert!(!launcher.results().is_empty());
        assert!(!launcher.offers_command());
        assert!(!launcher.visible.contains(&Target::Command));
    }

    #[test]
    fn nothing_typed_offers_no_run_row() {
        let (launcher, _) = typed("");
        assert!(!launcher.offers_command());
        assert!(!launcher.visible.contains(&Target::Command));
    }

    #[test]
    fn the_run_row_comes_after_the_files() {
        // "notes" is a file and not an application: the file is what was
        // most likely meant, and the shell is the fallback below it.
        let (mut launcher, apps) = with_files("notes");
        assert!(launcher.offers_command());
        assert!(matches!(launcher.target(), Some(Target::File(_))));
        launcher.press(Key::Down, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(launcher.target(), Some(Target::Command));
        assert_eq!(
            launcher.press(Key::Down, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Unchanged,
            "the run row is the last row"
        );
    }

    #[test]
    fn tab_on_the_run_row_does_nothing() {
        let (mut launcher, apps) = typed("qqzz");
        assert_eq!(launcher.target(), Some(Target::Command));
        assert_eq!(
            launcher.press(Key::Actions, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(launcher.menu(), None);
    }

    #[test]
    fn arithmetic_puts_its_value_first() {
        let (launcher, _) = typed("2+3*4");
        assert_eq!(launcher.result(), Some("14"));
        assert_eq!(launcher.target(), Some(Target::Result));
        assert_eq!(launcher.visible.first(), Some(&Target::Result));
    }

    #[test]
    fn enter_on_a_result_neither_launches_nor_closes() {
        // No clipboard to copy it to; the value stays where it can be read.
        let (mut launcher, apps) = typed("2+2");
        assert_eq!(launcher.target(), Some(Target::Result));
        assert_eq!(
            launcher.press(Key::Launch, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert!(launcher.is_open());
        assert_eq!(
            launcher.press(Key::Actions, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Unchanged,
            "a value has no actions"
        );
    }

    #[test]
    fn division_by_zero_shows_no_result_row() {
        let (launcher, _) = typed("1/0");
        assert_eq!(launcher.result(), None);
        assert!(!launcher.visible.contains(&Target::Result));
        // The fallback is still there: nothing matched, so it can be run.
        assert_eq!(launcher.target(), Some(Target::Command));
    }

    #[test]
    fn a_word_is_not_a_result() {
        let (launcher, _) = typed("fire");
        assert_eq!(launcher.result(), None);
        assert!(!launcher.visible.contains(&Target::Result));
    }

    #[test]
    fn down_from_the_result_lands_on_the_run_row() {
        // "2+2" matches nothing installed, so the list is the value and the
        // fallback, in that order.
        let (mut launcher, apps) = typed("2+2");
        launcher.press(Key::Down, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(launcher.target(), Some(Target::Command));
        launcher.press(Key::Up, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert_eq!(launcher.target(), Some(Target::Result));
    }

    #[test]
    fn the_result_goes_away_with_the_grid() {
        let (mut launcher, apps) = typed("2+2");
        launcher.press(Key::Clear, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert!(launcher.is_grid());
        assert_eq!(launcher.result(), None);
        assert!(!launcher.offers_command());
    }

    // ---- key mapping ----

    use smithay::input::keyboard::keysyms;

    #[test]
    fn characters_come_from_the_layout_not_from_the_keysym() {
        // Mapping KEY_a to 'a' here is correct on exactly one layout. The
        // character the keymap produced is what gets typed.
        assert_eq!(
            Key::from_keysym(keysyms::KEY_a, false, Some('a')),
            Key::Insert('a')
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_a, false, Some('ä')),
            Key::Insert('ä')
        );
    }

    #[test]
    fn control_chords_never_leak_a_character_into_the_query() {
        // Ctrl+S has a character on some layouts. Typing an 's' because of it
        // would corrupt the query on a keystroke meant for something else.
        assert_eq!(
            Key::from_keysym(keysyms::KEY_s, true, Some('s')),
            Key::Ignored
        );
    }

    #[test]
    fn the_editing_chords_are_the_ones_people_already_know() {
        assert_eq!(
            Key::from_keysym(keysyms::KEY_u, true, Some('u')),
            Key::Clear
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_w, true, Some('w')),
            Key::DeleteWord
        );
        assert_eq!(Key::from_keysym(keysyms::KEY_p, true, Some('p')), Key::Up);
        assert_eq!(Key::from_keysym(keysyms::KEY_n, true, Some('n')), Key::Down);
        assert_eq!(
            Key::from_keysym(keysyms::KEY_BackSpace, true, None),
            Key::DeleteWord
        );
    }

    #[test]
    fn escape_and_return_are_recognised_without_a_character() {
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Escape, false, None),
            Key::Dismiss
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Return, false, None),
            Key::Launch
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_KP_Enter, false, None),
            Key::Launch
        );
    }

    #[test]
    fn a_control_character_is_never_typed_into_the_query() {
        // A keysym can carry \t or \r as its character; appending either would
        // put an invisible character in the search field.
        for c in ['\t', '\r', '\n', '\u{7f}'] {
            assert_eq!(
                Key::from_keysym(keysyms::KEY_a, false, Some(c)),
                Key::Ignored
            );
        }
    }

    #[test]
    fn an_unknown_key_with_no_character_is_ignored_rather_than_forwarded() {
        // Still swallowed by the caller: a modifier press reaching the focused
        // client while the launcher is open lets a window act on a chord the
        // user was typing at the launcher.
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Shift_L, false, None),
            Key::Ignored
        );
    }

    #[test]
    fn reindexing_keeps_the_highlight_on_the_application_it_was_on() {
        // Results are indices into the application list. An install shifts
        // every index after it, so a launcher left un-reindexed highlights one
        // application and launches the one that took its place.
        let (mut launcher, apps) = typed("fi");
        let chosen = apps[launcher.selection().expect("a selection")]
            .name
            .clone();

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
            launcher
                .results
                .iter()
                .all(|i| apps[*i].name.starts_with("Fi")
                    || apps[*i].name.contains("fi")
                    || apps[*i].name.starts_with("Fr")),
            "the query was lost and everything matched",
        );
    }

    #[test]
    fn an_index_arriving_leaves_the_highlight_on_the_command_row() {
        // The Run row is the last thing in navigation order. A background
        // index that finds files for the query slots them in above it, and
        // a highlight kept by position would slide off Run onto a file the
        // user never looked at, one Enter away from opening it.
        let (mut launcher, apps) = typed("photo");
        let frecency = Frecency::new();
        assert_eq!(launcher.target(), Some(Target::Command));

        launcher.set_files(std::sync::Arc::new(FileIndex::from_paths(
            std::path::Path::new("/home/u"),
            [PathBuf::from("/home/u/photo.jpg")],
        )));
        launcher.reindex(&apps, &frecency, NOW);
        assert_eq!(launcher.file_hits().len(), 1);
        assert_eq!(launcher.target(), Some(Target::Command));
    }

    #[test]
    fn a_menu_survives_a_reindex_that_leaves_its_entry_in_place() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.menu(), Some(0));

        launcher.reindex(&apps, &frecency, NOW);
        assert_eq!(launcher.menu(), Some(0));
        assert_eq!(launcher.selection(), Some(0));
    }

    #[test]
    fn a_menu_closes_when_a_reindex_takes_its_entry_away() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);

        let empty: Vec<Entry> = Vec::new();
        launcher.reindex(&empty, &frecency, NOW);
        assert_eq!(launcher.menu(), None);
    }

    #[test]
    fn typing_after_a_reindex_still_resets_to_the_first_result() {
        // Only the background re-rank keeps the highlight where it was; a
        // keystroke is still a new question with its best answer on top.
        let (mut launcher, apps) = typed("");
        let frecency = Frecency::new();
        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.reindex(&apps, &frecency, NOW);
        assert_ne!(launcher.selected, 0);

        launcher.press(Key::Insert('f'), &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selected, 0);
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

    // -- The pointer -------------------------------------------------------

    /// A layout with one row per navigable target, stacked, each 100 wide
    /// and 20 tall from the top — stood in for a real composition, which
    /// the render tests cover.
    fn stacked(count: usize) -> Layout {
        Layout {
            size: (100, 20 * count as i32 + 40),
            hits: (0..count)
                .map(|i| (Rect::from_xywh(0, 20 * i as i32, 100, 20), i))
                .collect(),
            menu_hits: Vec::new(),
        }
    }

    fn on_row(row: usize) -> huginn_core::geometry::Point {
        huginn_core::geometry::Point::new(50, 20 * row as i32 + 10)
    }

    #[test]
    fn hovering_a_target_moves_the_highlight_to_it() {
        let (mut launcher, apps) = typed("");
        launcher.set_layout(stacked(apps.len()));
        assert_eq!(launcher.hover(on_row(2)), Outcome::Redraw);
        assert_eq!(launcher.selected, 2);
        // Still there: nothing to redraw.
        assert_eq!(launcher.hover(on_row(2)), Outcome::Unchanged);
    }

    #[test]
    fn hovering_a_scrolled_list_does_not_scroll_it() {
        // Down to the last application: the window is at the bottom. The
        // pointer landing on the top drawn row must highlight that row and
        // leave the window alone, or the rows slide out from under it.
        let (mut launcher, apps) = with_many_tools();
        let frecency = Frecency::new();
        for _ in 0..11 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        let before = launcher.window().to_vec();
        assert_eq!(before.len(), VISIBLE);
        let top = before[0];
        // Rows numbered as `visible` is: the top drawn row is the selection
        // index of that application.
        let row = launcher
            .visible
            .iter()
            .position(|t| *t == Target::App(top))
            .unwrap();
        launcher.set_layout(stacked(launcher.visible.len()));
        assert_eq!(launcher.hover(on_row(row)), Outcome::Redraw);
        assert_eq!(launcher.target(), Some(Target::App(top)));
        assert_eq!(launcher.window(), &before[..], "hovering scrolled the list");
        // The keyboard still pulls the window: Up off the top slides it.
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.window()[1..], before[..VISIBLE - 1]);
    }

    #[test]
    fn hovering_nothing_leaves_the_highlight_where_it_was() {
        // Over the field, a heading, the footer: the highlight has nowhere
        // better to be, and a highlight that vanished would leave Enter
        // with nothing to launch.
        let (mut launcher, apps) = typed("");
        launcher.set_layout(stacked(apps.len()));
        launcher.hover(on_row(1));
        let below = huginn_core::geometry::Point::new(50, 20 * apps.len() as i32 + 30);
        assert_eq!(launcher.hover(below), Outcome::Unchanged);
        assert_eq!(launcher.selected, 1);
    }

    #[test]
    fn clicking_a_target_launches_it() {
        // The same path as Enter, so the click gets the same credit.
        let (mut launcher, apps) = typed("");
        launcher.set_layout(stacked(apps.len()));
        let third = apps[launcher.results()[2]].path.clone();
        match launcher.click(on_row(2), &apps, CLOCK, STILL) {
            Outcome::Launch { entry, .. } => assert_eq!(entry, Some(third)),
            other => panic!("a click launched nothing: {other:?}"),
        }
        assert!(!launcher.is_open(), "it stayed open after launching");
    }

    #[test]
    fn clicking_nothing_launches_nothing() {
        let (mut launcher, apps) = typed("");
        launcher.set_layout(stacked(apps.len()));
        let below = huginn_core::geometry::Point::new(50, 20 * apps.len() as i32 + 30);
        assert_eq!(
            launcher.click(below, &apps, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert!(launcher.is_open(), "a click on nothing closed it");
    }

    #[test]
    fn a_closed_launcher_ignores_the_pointer() {
        let (mut launcher, apps) = typed("");
        launcher.set_layout(stacked(apps.len()));
        launcher.close(CLOCK, STILL);
        assert_eq!(launcher.hover(on_row(1)), Outcome::Unchanged);
        assert_eq!(
            launcher.click(on_row(1), &apps, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn a_stale_layout_cannot_point_past_the_list() {
        // The layout is from the last redraw; the list may have shrunk
        // since. A hit past the end is ignored rather than trusted.
        let (mut launcher, apps) = typed("");
        launcher.set_layout(stacked(apps.len() + 3));
        assert_eq!(launcher.hover(on_row(apps.len() + 1)), Outcome::Unchanged);
        assert_eq!(launcher.selected, 0);
    }

    /// The stacked rows with a menu of `items` drawn over the right half
    /// of the last two rows.
    fn with_menu(count: usize, items: usize) -> Layout {
        let mut layout = stacked(count);
        layout.menu_hits = (0..items)
            .map(|n| (Rect::from_xywh(50, 20 * n as i32, 50, 20), n))
            .collect();
        layout
    }

    fn on_menu(item: usize) -> huginn_core::geometry::Point {
        huginn_core::geometry::Point::new(75, 20 * item as i32 + 10)
    }

    #[test]
    fn with_the_menu_up_hover_moves_the_menus_highlight_and_not_the_selection() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.set_layout(with_menu(1, 3));
        assert_eq!(launcher.hover(on_menu(2)), Outcome::Redraw);
        assert_eq!(launcher.menu(), Some(2));
        assert_eq!(launcher.selected, 0);
        // Beside the menu: the menu keeps its item, the list its selection.
        let beside = huginn_core::geometry::Point::new(25, 10);
        assert_eq!(launcher.hover(beside), Outcome::Unchanged);
        assert_eq!(launcher.menu(), Some(2));
    }

    #[test]
    fn clicking_a_menu_item_runs_that_action() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.set_layout(with_menu(1, 3));
        match launcher.click(on_menu(1), &apps, CLOCK, STILL) {
            Outcome::Launch { argv, .. } => {
                assert_eq!(argv, vec!["/bin/browser", "--new-window"]);
            }
            other => panic!("the menu item did not run: {other:?}"),
        }
    }

    #[test]
    fn clicking_beside_the_menu_puts_it_away() {
        // As Escape does — not launching whatever was under the menu's edge.
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.set_layout(with_menu(1, 3));
        let beside = huginn_core::geometry::Point::new(25, 10);
        assert_eq!(launcher.click(beside, &apps, CLOCK, STILL), Outcome::Redraw);
        assert_eq!(launcher.menu(), None);
        assert!(
            launcher.is_open(),
            "putting the menu away closed the launcher"
        );
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

use crate::canvas::{Canvas, Panel};
use crate::text::Text;
use huginn_core::geometry::{Point, Rect};

/// Where [`compose`] put everything the pointer can land on.
///
/// Everything is in the canvas's own pixels — the coordinate space the panel
/// was drawn in, before the renderer scales it onto the output — and maps to
/// a position in the launcher's navigation order, the same index the arrow
/// keys move through. Recorded as the rows are drawn rather than computed
/// again from the same measurements: two layouts are two chances to disagree
/// about where a row is, and a highlight that lands one row off where the
/// pointer is would be found by clicking, not by reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The canvas's size, so a pointer over the scaled panel can be put back
    /// into canvas pixels. See [`Self::canvas_point`].
    pub(crate) size: (i32, i32),
    /// Each navigable target — a tile, a recent row, a result row, a file
    /// row, the result or run row — with its index into the navigation order.
    pub(crate) hits: Vec<(Rect, usize)>,
    /// The actions menu's items, when the menu is up, with the item number
    /// as [`Launcher::menu`] counts them. Checked first: the menu is drawn
    /// over the rows, so it is what a click there lands on.
    pub(crate) menu_hits: Vec<(Rect, usize)>,
}

impl Layout {
    /// The navigation index under `point`, if a target is there. The menu
    /// covers whatever it is drawn over, so a point on the menu is on no row.
    pub(crate) fn hit(&self, point: Point) -> Option<usize> {
        if self.menu_hit(point).is_some() {
            return None;
        }
        self.hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, index)| *index)
    }

    /// The menu item under `point`, if the menu is up and one is there.
    pub(crate) fn menu_hit(&self, point: Point) -> Option<usize> {
        self.menu_hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, item)| *item)
    }

    /// A pointer at `point` on the output, over the panel drawn at `panel`,
    /// as a canvas pixel — or `None` when it is not over the panel at all.
    ///
    /// The panel is placed at a fraction of its size while it opens (see
    /// [`placement`]) and at the output's density always, so the pointer is
    /// scaled by the ratio of the canvas to the rectangle it was drawn into
    /// rather than offset by the rectangle's corner alone.
    pub(crate) fn canvas_point(&self, panel: Rect, point: Point) -> Option<Point> {
        if panel.is_empty() || !panel.contains(point) {
            return None;
        }
        let x = (point.x - panel.x()) as f32 * self.size.0 as f32 / panel.w() as f32;
        let y = (point.y - panel.y()) as f32 * self.size.1 as f32 / panel.h() as f32;
        Some(Point::new(x as i32, y as i32))
    }
}

/// Panel width at a 1080p output, in pixels.
pub(crate) const WIDTH: f32 = 560.0;
/// Padding inside the panel's border.
pub(crate) const PAD: f32 = 18.0;
/// Text size at a 1080p output.
pub(crate) const BASE_SIZE: f32 = 16.0;
/// How many results are shown. Beyond this the answer was not in the list and
/// another keystroke is faster than another screenful.
const VISIBLE: usize = 8;
/// How many suggestions the grid shows before anything is typed.
const SUGGESTED: usize = 6;
/// Tiles per row of the suggestion grid.
const COLUMNS: usize = 3;
/// How many recently launched applications are listed under the grid.
const RECENT: usize = 3;
/// How many matching files are listed under the results. None are listed
/// until the last query term is [`raven_desktop::files::MIN_TERM`]
/// characters long; see [`Launcher::refresh`].
const FILES: usize = 4;
/// How much of a result row's inner width the application's kind ("Web
/// Browser") may take, at most, before it is cut. The name is what was
/// asked for; the kind is why the row answered, and stays the smaller half.
pub(crate) const KIND_SHARE: f32 = 0.45;
/// The theme icon drawn beside a file.
const FILE_ICON: &str = "text-x-generic";
/// Height of a suggestion tile at a 1080p output.
pub(crate) const TILE: f32 = 104.0;
/// Space between tiles.
pub(crate) const TILE_GAP: f32 = 12.0;
/// Corner radius of the panel.
pub(crate) const RADIUS: f32 = 22.0;
/// Opacity of the panel's background.
///
/// Low enough that the blurred desktop behind the panel (see
/// [`blur_rect`]) shows through as a frosted tint, high enough that text
/// stays legible over a busy wallpaper. At the old `0xF2` the blur was there
/// and invisible: a 95%-opaque panel hides whatever is behind it, blurred or
/// not.
pub(crate) const ALPHA: u8 = 0xD8;
// Legibility bounds the alpha from below; above the upper bound the panel
// hides the blur behind it and the blur pass is pure cost.
const _: () = assert!(ALPHA >= 0xC0 && ALPHA <= 0xE0);
/// The tile and field fill: a shade lighter than the panel, so they read as
/// wells set into it rather than as lines drawn on it.
pub(crate) const WELL_ALPHA: u8 = 0x70;
/// The footer's key hints, in the order they are read. The grid also
/// answers to sideways arrows, and says so; the menu says what it does.
const GRID_HINTS: &[(&str, &str)] = &[
    ("←↑↓→", "Navigate"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Esc", "Close"),
];
const LIST_HINTS: &[(&str, &str)] = &[
    ("↑↓", "Navigate"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Esc", "Close"),
];
const MENU_HINTS: &[(&str, &str)] = &[("↑↓", "Choose"), ("Enter", "Run"), ("Esc", "Back")];
/// On the "Run" row Enter runs a shell command, not an application, and the
/// footer says so; Tab has nothing to offer a command and is not listed.
const COMMAND_HINTS: &[(&str, &str)] = &[("↑↓", "Navigate"), ("Enter", "Run"), ("Esc", "Close")];
/// On the result row Enter does nothing — see [`Launcher::launch`] — and a
/// footer promising "Open" for it would be a lie.
const RESULT_HINTS: &[(&str, &str)] = &[("↑↓", "Navigate"), ("Esc", "Close")];
/// On a file row Enter opens the file, but Tab has no actions menu to
/// offer — actions belong to desktop entries — so it is left out rather
/// than advertised as a key that does nothing.
const FILE_HINTS: &[(&str, &str)] = &[("↑↓", "Navigate"), ("Enter", "Open"), ("Esc", "Close")];

/// Which hints the footer shows, chosen from what the highlight is on.
///
/// The footer is a promise about what the keys do, and the keys do
/// different things on different rows: Tab opens the actions menu on an
/// application and nothing anywhere else, Enter runs a command on the run
/// row and does nothing on the result row. So the hints follow the target,
/// and only the menu — which takes every key while it is up — overrides
/// them. A pure function so the choice can be tested without drawing.
fn hints_for(
    target: Option<Target>,
    menu_open: bool,
    grid: bool,
) -> &'static [(&'static str, &'static str)] {
    if menu_open {
        return MENU_HINTS;
    }
    match target {
        Some(Target::File(_)) => FILE_HINTS,
        Some(Target::Command) => COMMAND_HINTS,
        Some(Target::Result) => RESULT_HINTS,
        _ if grid => GRID_HINTS,
        _ => LIST_HINTS,
    }
}
/// What stands in for an icon on the "Run" row: the prompt character, which
/// is what "this goes to a shell" has looked like for fifty years.
const COMMAND_GLYPH: &str = ">";
/// And on the result row: the equals sign, so the row reads "= 4" with the
/// label carrying only the value; the label must not repeat the "=" or the
/// row would read "=  = 4".
const RESULT_GLYPH: &str = "=";

/// What is shown before anything has been typed.
const PLACEHOLDER: &str = "Search applications and files";
/// The actions menu's last item, which puts the entry on the pinned panel
/// — or takes it off. See [`crate::pinned`].
pub(crate) const PIN: &str = "Pin";
pub(crate) const UNPIN: &str = "Unpin";

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
pub(crate) fn placement(
    output: Rect,
    panel: (i32, i32),
    origin: Option<Rect>,
    reveal: f32,
) -> Rect {
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
    let (fx, fy) = (from.x() + from.w() / 2, from.y() + from.h() / 2);
    let (tx, ty) = (full.x() + full.w() / 2, full.y() + full.h() / 2);
    let cx = fx + ((tx - fx) as f32 * t) as i32;
    let cy = fy + ((ty - fy) as f32 * t) as i32;

    Rect::from_xywh(cx - sw / 2, cy - sh / 2, sw, sh)
}

/// The region of the desktop the panel at `placement` blurs.
///
/// The blur is cut out of the desktop with a rectangle, and the panel's
/// corners are rounded: a blur the full size of the panel would show as four
/// square, blurred corners peeking out past the rounding, since the panel's
/// corner pixels are transparent and hide nothing. The blur path cannot mask
/// a corner, so the rectangle is inset by the corner radius instead. What
/// that costs is a strip along each edge, one radius wide, where the panel
/// tints the desktop without softening it — at [`ALPHA`] the difference is
/// barely there, and it beats the alternative.
///
/// `None` when the panel is too small for a blur to fit inside the inset —
/// the first frames of the reveal from a dock icon, or a panel that has been
/// shrunk to nothing — so the renderer takes the ordinary path rather than
/// cropping to an inverted rectangle.
pub(crate) fn blur_rect(placement: Rect) -> Option<Rect> {
    let inner = placement.inset(RADIUS as i32);
    (!inner.is_empty()).then_some(inner)
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
) -> (Panel, Layout) {
    let (canvas, layout) = compose(launcher, apps, text, icons, pixmaps, output, density);
    (Panel::from_canvas(&canvas, density), layout)
}

/// Lay the launcher out and paint it, and say where everything went. Split
/// from [`render`] so a test can get at the pixels without going through a
/// renderer.
fn compose(
    launcher: &Launcher,
    apps: &[Entry],
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    output: Rect,
    density: u32,
) -> (Canvas, Layout) {
    // Everything here is in the canvas's own pixels, which `density` makes
    // more numerous than the logical ones the panel is placed in. See
    // `Panel::from_canvas`.
    let m = Metrics::for_output(output, density);
    let Metrics {
        density,
        scale,
        size,
        pad,
        width,
        row,
        field,
        radius,
        inner,
        gap,
        heading,
        footer,
    } = m;
    // A menu of `items` actions: its heading, the rows, a little air below.
    let entry_menu_h = |items: usize| m.menu_height(items);

    // The grid before anything is typed, the list once something is: the
    // panel is as tall as whichever it is showing.
    let grid = launcher.is_grid();
    let tiles = launcher.results().len().min(SUGGESTED);
    let tile_rows = tiles.div_ceil(COLUMNS);
    let tile_w = (inner - gap * (COLUMNS - 1) as f32) / COLUMNS as f32;
    let tile_h = TILE * scale;
    let shown = launcher.window().len();
    let recent = launcher.recent().len();
    let body = if grid {
        let tiles = if tiles > 0 {
            heading + tile_rows as f32 * tile_h + (tile_rows - 1) as f32 * gap + gap
        } else {
            0.0
        };
        let rows = if recent > 0 {
            gap + heading + row * recent as f32 + gap
        } else {
            0.0
        };
        tiles + rows
    } else {
        let files = launcher.file_hits().len();
        let result = if launcher.result().is_some() {
            row + gap
        } else {
            0.0
        };
        let command = if launcher.offers_command() {
            gap + row + gap
        } else {
            0.0
        };
        result
            + row * shown as f32
            + if files > 0 {
                gap + heading + row * files as f32 + gap
            } else {
                0.0
            }
            + command
    };
    // The actions menu sits over the body, and a short list leaves it
    // nowhere to go but the footer: the body is at least as tall as the menu.
    let menu = launcher
        .menu()
        .and(launcher.selection())
        .and_then(|i| apps.get(i))
        .map(|entry| entry_menu_h(launcher.menu_items(entry).len()));
    let body = body.max(menu.unwrap_or(0.0));
    let height = (pad * 2.0 + field + gap + body + footer) as usize;

    let mut canvas = Canvas::new(width, height.max(1));
    let mut layout = Layout {
        size: (width as i32, height.max(1) as i32),
        hits: Vec::new(),
        menu_hits: Vec::new(),
    };
    // A row's place in the navigation order, for the pointer. The list's
    // rows are drawn from `window`, which is a slice of `results`, and the
    // order the keys walk is `visible`; this is what joins the two.
    let slot_of = |target: Target| launcher.visible.iter().position(|t| *t == target);
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    canvas.fill_rounded(
        0,
        0,
        width,
        height,
        radius,
        crate::theme::BACKGROUND.with_alpha(ALPHA),
    );

    // The field, as a pill set into the panel.
    canvas.fill_rounded(
        pad as usize,
        pad as usize,
        inner as usize,
        field as usize,
        field / 2.0,
        crate::theme::BORDER.with_alpha(WELL_ALPHA),
    );

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
    let field_x = pad + field / 2.0;
    // The placeholder starts clear of the caret. Both at the same x puts a
    // bar through the first letter of the hint, which reads as a rendering
    // fault rather than as a cursor.
    let text_x = if launcher.query().is_empty() {
        field_x + caret_w + 4.0 * scale
    } else {
        field_x
    };
    text.draw(
        &mut canvas,
        query_text,
        size * 1.25,
        text_x as i32,
        text_y,
        query_color,
    );

    // A caret, so the field reads as focused even when it is empty.
    // Measured from the query, not the placeholder, so an empty field puts the
    // caret at the start rather than after the hint text.
    let caret_x = field_x + text.measure(launcher.query(), size * 1.25).0 + 2.0;
    canvas.fill(
        caret_x as usize,
        text_y as usize,
        caret_w as usize,
        (size * 1.4) as usize,
        crate::theme::accent().to_rgba_bytes(),
    );

    // Position in navigation order — a tile, a row, or a file row.
    let selected = launcher.selected;
    // And what it is on, for the list, whose rows are of four kinds and
    // whose order depends on which of them are showing.
    let target = launcher.target();
    let style = RowStyle {
        pad,
        inner,
        row,
        size,
        scale,
    };

    let mut y = pad + field + gap;
    if grid {
        if tiles > 0 {
            text.draw(
                &mut canvas,
                "Suggested",
                size * 0.95,
                pad as i32,
                (y + (heading - size * 1.3) / 2.0) as i32,
                crate::theme::TEXT_DIM,
            );
            y += heading;
        }
        for (slot, index) in launcher.results().iter().take(SUGGESTED).enumerate() {
            let Some(entry) = apps.get(*index) else {
                continue;
            };
            let (col, row_n) = (slot % COLUMNS, slot / COLUMNS);
            let x = pad + (tile_w + gap) * col as f32;
            let ty = y + (tile_h + gap) * row_n as f32;
            layout.hits.push((rect(x, ty, tile_w, tile_h), slot));
            draw_tile(
                &mut canvas,
                text,
                icons,
                pixmaps,
                &m,
                entry,
                x,
                ty,
                tile_w,
                tile_h,
                slot == selected,
            );
        }
        if tiles > 0 {
            y += tile_rows as f32 * tile_h + (tile_rows - 1) as f32 * gap + gap;
        }

        if recent > 0 {
            canvas.fill(
                pad as usize,
                y as usize,
                inner as usize,
                1,
                crate::theme::BORDER.to_rgba_bytes(),
            );
            y += gap;
            text.draw(
                &mut canvas,
                "Recent",
                size * 0.95,
                pad as i32,
                (y + (heading - size * 1.3) / 2.0) as i32,
                crate::theme::TEXT_DIM,
            );
            y += heading;
            let icon_size = (size * 1.5) as u32;
            let icon_x = pad + 8.0 * scale;
            let stamp_size = size * 0.85;
            let chevron_w = text.measure("›", size).0;
            for (slot, (index, at)) in launcher.recent().iter().enumerate() {
                let Some(entry) = apps.get(*index) else {
                    continue;
                };
                let highlighted = tiles + slot == selected;
                layout.hits.push((rect(pad, y, inner, row), tiles + slot));
                if highlighted {
                    canvas.fill_rounded(
                        pad as usize,
                        y as usize,
                        inner as usize,
                        row as usize,
                        row * 0.25,
                        crate::theme::accent().with_alpha(0x2E),
                    );
                }
                if let Some(pixmap) = entry
                    .icon
                    .as_deref()
                    .and_then(|name| launcher_icon(icons, name, icon_size / density, density))
                    .and_then(|path| pixmaps.get(&path, icon_size))
                {
                    canvas.blit(
                        icon_x as usize,
                        (y + (row - icon_size as f32) / 2.0) as usize,
                        &tinted(pixmap),
                    );
                }
                let color = if highlighted {
                    crate::theme::TEXT
                } else {
                    crate::theme::TEXT_DIM
                };
                // When, then a chevron, against the right edge: the row is
                // something to open, and the mark says so.
                let stamp = ago(launcher.now().saturating_sub(*at));
                let right = pad + inner - 10.0 * scale;
                text.draw(
                    &mut canvas,
                    "›",
                    size,
                    (right - chevron_w) as i32,
                    (y + (row - size * 1.35) / 2.0) as i32,
                    crate::theme::TEXT_DIM,
                );
                let (sw, _) = text.measure(&stamp, stamp_size);
                let stamp_x = right - chevron_w - 12.0 * scale - sw;
                text.draw(
                    &mut canvas,
                    &stamp,
                    stamp_size,
                    stamp_x as i32,
                    (y + (row - stamp_size * 1.35) / 2.0) as i32,
                    crate::theme::TEXT_DIM,
                );
                // What it is, before the stamp, when the name leaves room
                // for it. Nothing here was searched for, so the kind is a
                // courtesy rather than an explanation: a name that fills the
                // row wins, and a kind that would not fit whole is dropped
                // rather than cut, because "Web Br…" explains nothing.
                let name_x = icon_x + icon_size as f32 + 10.0 * scale;
                let (nw, _) = text.measure(&entry.name, size);
                let mut name_room = stamp_x - gap - name_x;
                if let Some(kind) = kind_of(entry) {
                    let (kw, _) = text.measure(kind, stamp_size);
                    let kind_x = stamp_x - gap - kw;
                    if kw <= inner * KIND_SHARE && kind_x - gap >= name_x + nw {
                        text.draw(
                            &mut canvas,
                            kind,
                            stamp_size,
                            kind_x as i32,
                            (y + (row - stamp_size * 1.35) / 2.0) as i32,
                            crate::theme::TEXT_DIM,
                        );
                        name_room = kind_x - gap - name_x;
                    }
                }
                let name = fit(text, &entry.name, size, name_room);
                text.draw(
                    &mut canvas,
                    &name,
                    size,
                    name_x as i32,
                    (y + (row - size * 1.35) / 2.0) as i32,
                    color,
                );
                y += row;
            }
            y += gap;
        }
    } else {
        // The answer, before anything that merely matched. A hairline under
        // it separates a value from the rows that are things to open.
        if let Some(value) = launcher.result() {
            if let Some(slot) = slot_of(Target::Result) {
                layout.hits.push((rect(pad, y, inner, row), slot));
            }
            glyph_row(
                &mut canvas,
                text,
                &style,
                y,
                RESULT_GLYPH,
                value,
                target == Some(Target::Result),
            );
            y += row;
            canvas.fill(
                pad as usize,
                y as usize,
                inner as usize,
                1,
                crate::theme::BORDER.to_rgba_bytes(),
            );
            y += gap;
        }

        for index in launcher.window() {
            let Some(entry) = apps.get(*index) else {
                continue;
            };
            let highlighted = target == Some(Target::App(*index));
            if let Some(slot) = slot_of(Target::App(*index)) {
                layout.hits.push((rect(pad, y, inner, row), slot));
            }
            draw_app_row(&mut canvas, text, icons, pixmaps, &m, entry, y, highlighted);
            y += row;
        }

        if !launcher.file_hits().is_empty() {
            canvas.fill(
                pad as usize,
                y as usize,
                inner as usize,
                1,
                crate::theme::BORDER.to_rgba_bytes(),
            );
            y += gap;
            text.draw(
                &mut canvas,
                "Files",
                size * 0.95,
                pad as i32,
                (y + (heading - size * 1.3) / 2.0) as i32,
                crate::theme::TEXT_DIM,
            );
            y += heading;
            let icon_size = (size * 1.5) as u32;
            let icon_x = pad + 8.0 * scale;
            let where_size = size * 0.85;
            let chevron_w = text.measure("›", size).0;
            let file_icon = launcher_icon(icons, FILE_ICON, icon_size / density, density)
                .and_then(|path| pixmaps.get(&path, icon_size))
                .map(tinted);
            for index in launcher.file_hits() {
                let Some(file) = launcher.files().get(*index) else {
                    continue;
                };
                let highlighted = target == Some(Target::File(*index));
                if let Some(slot) = slot_of(Target::File(*index)) {
                    layout.hits.push((rect(pad, y, inner, row), slot));
                }
                if highlighted {
                    canvas.fill_rounded(
                        pad as usize,
                        y as usize,
                        inner as usize,
                        row as usize,
                        row * 0.25,
                        crate::theme::accent().with_alpha(0x2E),
                    );
                }
                if let Some(icon) = &file_icon {
                    canvas.blit(
                        icon_x as usize,
                        (y + (row - icon_size as f32) / 2.0) as usize,
                        icon,
                    );
                }
                let color = if highlighted {
                    crate::theme::TEXT
                } else {
                    crate::theme::TEXT_DIM
                };
                // Where it is, then a chevron, against the right edge; the
                // name is cut before it can run into either.
                let right = pad + inner - 10.0 * scale;
                text.draw(
                    &mut canvas,
                    "›",
                    size,
                    (right - chevron_w) as i32,
                    (y + (row - size * 1.35) / 2.0) as i32,
                    crate::theme::TEXT_DIM,
                );
                let location = fit(
                    text,
                    &launcher.files().location(*index),
                    where_size,
                    inner * 0.4,
                );
                let (lw, _) = text.measure(&location, where_size);
                let location_x = right - chevron_w - 12.0 * scale - lw;
                text.draw(
                    &mut canvas,
                    &location,
                    where_size,
                    location_x as i32,
                    (y + (row - where_size * 1.35) / 2.0) as i32,
                    crate::theme::TEXT_DIM,
                );
                let name_x = icon_x + icon_size as f32 + 10.0 * scale;
                let name = fit(text, &file.name, size, location_x - gap - name_x);
                text.draw(
                    &mut canvas,
                    &name,
                    size,
                    name_x as i32,
                    (y + (row - size * 1.35) / 2.0) as i32,
                    color,
                );
                y += row;
            }
            y += gap;
        }

        // The fallback, last: run what was typed. Under a hairline of its
        // own so it reads as a different kind of offer from the files.
        if launcher.offers_command() {
            canvas.fill(
                pad as usize,
                y as usize,
                inner as usize,
                1,
                crate::theme::BORDER.to_rgba_bytes(),
            );
            y += gap;
            let label = fit(
                text,
                &format!("Run \"{}\"", launcher.query()),
                size,
                inner - (pad + 8.0 * scale + size * 1.5 + 10.0 * scale) - 10.0 * scale,
            );
            if let Some(slot) = slot_of(Target::Command) {
                layout.hits.push((rect(pad, y, inner, row), slot));
            }
            glyph_row(
                &mut canvas,
                text,
                &style,
                y,
                COMMAND_GLYPH,
                &label,
                target == Some(Target::Command),
            );
            y += row + gap;
        }
    }

    // The actions menu, over the bottom of the body and against the right
    // edge — near the footer hint that summoned it, and clear of the field.
    // Drawn last so it sits on top of whatever it covers.
    if let (Some(item), Some(entry)) = (
        launcher.menu(),
        launcher.selection().and_then(|i| apps.get(i)),
    ) {
        let items = launcher.menu_items(entry);
        draw_menu(
            &mut canvas,
            text,
            &mut layout,
            &m,
            &entry.name,
            &items,
            item,
            y,
            pad + field + gap,
        );
    }

    draw_footer(
        &mut canvas,
        text,
        &m,
        y,
        hints_for(target, launcher.menu().is_some(), grid),
    );

    (canvas, layout)
}

/// The measurements every panel that looks like the launcher is laid out
/// with, in canvas pixels.
///
/// One struct rather than a dozen `let`s at the top of each `compose`, so the
/// pinned panel (see [`crate::pinned`]) is drawn to exactly the launcher's
/// proportions rather than to a copy of them that drifts by a constant.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Metrics {
    /// Canvas pixels per logical pixel, at least 1.
    pub(crate) density: u32,
    /// Everything below is multiplied by this: the 1080p design size, times
    /// how much taller the output is, times the density.
    pub(crate) scale: f32,
    /// Body text size.
    pub(crate) size: f32,
    /// Padding inside the panel's border.
    pub(crate) pad: f32,
    /// The panel's width.
    pub(crate) width: usize,
    /// Height of a list row.
    pub(crate) row: f32,
    /// Height of the search field.
    pub(crate) field: f32,
    /// Corner radius of the panel.
    pub(crate) radius: f32,
    /// The width inside the padding.
    pub(crate) inner: f32,
    /// Space between tiles, and between sections.
    pub(crate) gap: f32,
    /// Height of a section heading.
    pub(crate) heading: f32,
    /// Height of the footer.
    pub(crate) footer: f32,
}

impl Metrics {
    /// The measurements for `output` at `density` pixels per logical one.
    pub(crate) fn for_output(output: Rect, density: u32) -> Self {
        let density = density.max(1);
        let scale = (output.h() as f32 / 1080.0).clamp(1.0, 2.5) * density as f32;
        let size = BASE_SIZE * scale;
        let pad = PAD * scale;
        let width = (WIDTH * scale) as usize;
        Self {
            density,
            scale,
            size,
            pad,
            width,
            row: size * 2.2,
            field: size * 2.6,
            radius: RADIUS * scale,
            inner: width as f32 - pad * 2.0,
            gap: TILE_GAP * scale,
            heading: size * 1.9,
            footer: size * 2.4,
        }
    }

    /// The same measurements for a panel `width` canvas pixels wide.
    pub(crate) fn with_width(self, width: usize) -> Self {
        Self {
            width,
            inner: width as f32 - self.pad * 2.0,
            ..self
        }
    }

    /// A menu of `items` actions: its heading, the rows, a little air below.
    pub(crate) fn menu_height(&self, items: usize) -> f32 {
        self.heading + self.row * items as f32 + self.gap
    }
}

/// Paint the panel's ground: the rounded, frosted rectangle everything else
/// sits on.
pub(crate) fn draw_ground(canvas: &mut Canvas, m: &Metrics, width: usize, height: usize) {
    canvas.fill_rounded(
        0,
        0,
        width,
        height,
        m.radius,
        crate::theme::BACKGROUND.with_alpha(ALPHA),
    );
}

/// A section heading — "Suggested", "Recent", "Files" — at `y`.
pub(crate) fn draw_heading(canvas: &mut Canvas, text: &mut Text, m: &Metrics, y: f32, label: &str) {
    text.draw(
        canvas,
        label,
        m.size * 0.95,
        m.pad as i32,
        (y + (m.heading - m.size * 1.3) / 2.0) as i32,
        crate::theme::TEXT_DIM,
    );
}

/// A hairline across the panel's inner width at `y`.
pub(crate) fn draw_rule(canvas: &mut Canvas, m: &Metrics, y: f32) {
    canvas.fill(
        m.pad as usize,
        y as usize,
        m.inner as usize,
        1,
        crate::theme::BORDER.to_rgba_bytes(),
    );
}

/// A suggestion tile: icon over label, in a well, ringed when selected.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tile(
    canvas: &mut Canvas,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    m: &Metrics,
    entry: &Entry,
    x: f32,
    ty: f32,
    tile_w: f32,
    tile_h: f32,
    selected: bool,
) {
    let Metrics {
        density,
        scale,
        size,
        gap,
        ..
    } = *m;
    let corner = tile_h * 0.16;
    if selected {
        // A ring: the accent drawn a little larger, and the tile's
        // own fill on top of it. Two fills rather than a stroke,
        // because the canvas has no stroke and this is the same
        // shape the dock's selection uses.
        let ring = (2.0 * scale).max(2.0);
        canvas.fill_rounded(
            (x - ring) as usize,
            (ty - ring) as usize,
            (tile_w + ring * 2.0) as usize,
            (tile_h + ring * 2.0) as usize,
            corner + ring,
            crate::theme::accent(),
        );
        canvas.fill_rounded(
            x as usize,
            ty as usize,
            tile_w as usize,
            tile_h as usize,
            corner,
            crate::theme::BACKGROUND,
        );
        canvas.fill_rounded(
            x as usize,
            ty as usize,
            tile_w as usize,
            tile_h as usize,
            corner,
            crate::theme::accent().with_alpha(0x2E),
        );
    } else {
        canvas.fill_rounded(
            x as usize,
            ty as usize,
            tile_w as usize,
            tile_h as usize,
            corner,
            crate::theme::BORDER.with_alpha(WELL_ALPHA),
        );
    }
    let icon_size = (size * 2.4) as u32;
    let label_size = size * 0.95;
    // Icon above label, the pair centred in the tile.
    let stack = icon_size as f32 + 6.0 * scale + label_size * 1.35;
    let top = ty + (tile_h - stack) / 2.0;
    if let Some(pixmap) = entry
        .icon
        .as_deref()
        .and_then(|name| launcher_icon(icons, name, icon_size / density, density))
        .and_then(|path| pixmaps.get(&path, icon_size))
    {
        canvas.blit(
            (x + (tile_w - icon_size as f32) / 2.0) as usize,
            top as usize,
            &tinted(pixmap),
        );
    }
    let label = fit(text, &entry.name, label_size, tile_w - gap);
    let (lw, _) = text.measure(&label, label_size);
    text.draw(
        canvas,
        &label,
        label_size,
        (x + (tile_w - lw) / 2.0) as i32,
        (top + icon_size as f32 + 6.0 * scale) as i32,
        if selected {
            crate::theme::TEXT
        } else {
            crate::theme::TEXT_DIM
        },
    );
}

/// A result row: icon, name, and what the application is against the right
/// edge, washed in the accent when highlighted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_app_row(
    canvas: &mut Canvas,
    text: &mut Text,
    icons: &Icons,
    pixmaps: &mut Pixmaps,
    m: &Metrics,
    entry: &Entry,
    y: f32,
    highlighted: bool,
) {
    let Metrics {
        density,
        scale,
        size,
        pad,
        row,
        inner,
        gap,
        ..
    } = *m;
    let kind_size = size * 0.85;
    if highlighted {
        canvas.fill_rounded(
            pad as usize,
            y as usize,
            inner as usize,
            row as usize,
            row * 0.25,
            crate::theme::accent().with_alpha(0x2E),
        );
    }
    // The icon, if the theme has one. An application with no icon
    // gets its name at the same indent as everything else rather
    // than shifted left, so the column of names stays a column.
    let icon_size = (size * 1.5) as u32;
    let icon_x = pad + 8.0 * scale;
    if let Some(pixmap) = entry
        .icon
        .as_deref()
        .and_then(|name| launcher_icon(icons, name, icon_size / density, density))
        .and_then(|path| pixmaps.get(&path, icon_size))
    {
        canvas.blit(
            icon_x as usize,
            (y + (row - icon_size as f32) / 2.0) as usize,
            &tinted(pixmap),
        );
    }
    // Why it matched, against the right edge. The query is run over
    // the generic name and the comment as well as the name, so a
    // search for "browser" lands on "Brave" — and without "Web
    // Browser" beside it the hit looks like a mistake. It also tells
    // three "Avahi ... Browser" rows apart. Cut before it can take
    // the row over, and the name cut before it can run into it.
    let name_x = icon_x + icon_size as f32 + 10.0 * scale;
    let right = pad + inner - 10.0 * scale;
    let mut name_room = right - name_x;
    if let Some(kind) = kind_of(entry) {
        let kind = fit(text, kind, kind_size, inner * KIND_SHARE);
        let (kw, _) = text.measure(&kind, kind_size);
        let kind_x = right - kw;
        text.draw(
            canvas,
            &kind,
            kind_size,
            kind_x as i32,
            (y + (row - kind_size * 1.35) / 2.0) as i32,
            crate::theme::TEXT_DIM,
        );
        name_room = kind_x - gap - name_x;
    }
    let name = fit(text, &entry.name, size, name_room);
    text.draw(
        canvas,
        &name,
        size,
        name_x as i32,
        (y + (row - size * 1.35) / 2.0) as i32,
        if highlighted {
            crate::theme::TEXT
        } else {
            crate::theme::TEXT_DIM
        },
    );
}

/// The actions menu: `labels` under `title`, with `item` highlighted, its
/// bottom edge at `bottom` and against the panel's right edge, never higher
/// than `top`. Records each item's rectangle in `layout` for the pointer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_menu(
    canvas: &mut Canvas,
    text: &mut Text,
    layout: &mut Layout,
    m: &Metrics,
    title: &str,
    labels: &[&str],
    item: usize,
    bottom: f32,
    top: f32,
) {
    let Metrics {
        scale,
        size,
        pad,
        row,
        inner,
        gap,
        heading,
        ..
    } = *m;
    let rect =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_xywh(x as i32, y as i32, w as i32, h as i32);
    let menu_w = inner * 0.5;
    let menu_h = m.menu_height(labels.len());
    let mx = pad + inner - menu_w;
    let my = (bottom - menu_h).max(top);
    let corner = row * 0.4;
    let edge = 1.0_f32.max(scale * 0.75);
    canvas.fill_rounded(
        (mx - edge) as usize,
        (my - edge) as usize,
        (menu_w + edge * 2.0) as usize,
        (menu_h + edge * 2.0) as usize,
        corner + edge,
        crate::theme::BORDER,
    );
    canvas.fill_rounded(
        mx as usize,
        my as usize,
        menu_w as usize,
        menu_h as usize,
        corner,
        crate::theme::BACKGROUND,
    );
    let title = fit(text, title, size * 0.85, menu_w - gap * 2.0);
    text.draw(
        canvas,
        &title,
        size * 0.85,
        (mx + gap) as i32,
        (my + (heading - size * 1.15) / 2.0) as i32,
        crate::theme::TEXT_DIM,
    );
    let mut iy = my + heading;
    for (n, label) in labels.iter().enumerate() {
        layout
            .menu_hits
            .push((rect(mx + gap / 2.0, iy, menu_w - gap, row), n));
        if n == item {
            canvas.fill_rounded(
                (mx + gap / 2.0) as usize,
                iy as usize,
                (menu_w - gap) as usize,
                row as usize,
                row * 0.25,
                crate::theme::accent().with_alpha(0x2E),
            );
        }
        let label = fit(text, label, size, menu_w - gap * 2.0);
        text.draw(
            canvas,
            &label,
            size,
            (mx + gap) as i32,
            (iy + (row - size * 1.35) / 2.0) as i32,
            if n == item {
                crate::theme::TEXT
            } else {
                crate::theme::TEXT_DIM
            },
        );
        iy += row;
    }
}

/// The footer: a hairline at `y`, then what the keys do. Chips for the keys
/// and dim text for the verbs, so the eye finds the key first.
pub(crate) fn draw_footer(
    canvas: &mut Canvas,
    text: &mut Text,
    m: &Metrics,
    y: f32,
    hints: &[(&str, &str)],
) {
    let Metrics {
        scale,
        size,
        pad,
        gap,
        footer,
        ..
    } = *m;
    draw_rule(canvas, m, y);
    let hint_size = size * 0.85;
    let chip_h = hint_size * 1.7;
    let chip_pad = 6.0 * scale;
    let mut hx = pad;
    let hy = y + (footer - chip_h) / 2.0 + 1.0;
    // The air between hints: two gaps when the row has room, and less when
    // it does not, down to a chip's padding — so a panel with one hint more
    // than the launcher's does not push its last verb off the edge.
    let taken: f32 = hints
        .iter()
        .map(|(key, verb)| {
            text.measure(key, hint_size).0 + chip_pad * 3.0 + text.measure(verb, hint_size).0
        })
        .sum();
    let between = hints.len().saturating_sub(1) as f32;
    let spacing = if between > 0.0 {
        ((m.inner - taken) / between).clamp(chip_pad, gap * 2.0)
    } else {
        gap * 2.0
    };
    for (key, verb) in hints {
        let (kw, _) = text.measure(key, hint_size);
        let chip_w = kw + chip_pad * 2.0;
        canvas.fill_rounded(
            hx as usize,
            hy as usize,
            chip_w as usize,
            chip_h as usize,
            4.0 * scale,
            crate::theme::BORDER,
        );
        text.draw(
            canvas,
            key,
            hint_size,
            (hx + chip_pad) as i32,
            (hy + (chip_h - hint_size * 1.35) / 2.0) as i32,
            crate::theme::TEXT,
        );
        hx += chip_w + chip_pad;
        text.draw(
            canvas,
            verb,
            hint_size,
            hx as i32,
            (hy + (chip_h - hint_size * 1.35) / 2.0) as i32,
            crate::theme::TEXT_DIM,
        );
        hx += text.measure(verb, hint_size).0 + spacing;
    }
}

/// The measurements a list row is laid out with, in canvas pixels.
///
/// One struct rather than five arguments, so [`glyph_row`] takes the same
/// geometry the application and file rows are drawn with and cannot drift
/// from it by one parameter.
struct RowStyle {
    pad: f32,
    inner: f32,
    row: f32,
    size: f32,
    scale: f32,
}

/// A list row whose icon is a character rather than a theme pixmap: the
/// result row and the "Run" row.
///
/// Same well, same highlight wash, same indent as the rows that carry a
/// pixmap: the glyph is centred where the icon would be, so the column of
/// labels stays a column whichever kinds of row are showing.
fn glyph_row(
    canvas: &mut Canvas,
    text: &mut Text,
    style: &RowStyle,
    y: f32,
    glyph: &str,
    label: &str,
    highlighted: bool,
) {
    let RowStyle {
        pad,
        inner,
        row,
        size,
        scale,
    } = *style;
    if highlighted {
        canvas.fill_rounded(
            pad as usize,
            y as usize,
            inner as usize,
            row as usize,
            row * 0.25,
            crate::theme::accent().with_alpha(0x2E),
        );
    }
    let icon_size = size * 1.5;
    let icon_x = pad + 8.0 * scale;
    let glyph_size = size * 1.2;
    let (gw, _) = text.measure(glyph, glyph_size);
    text.draw(
        canvas,
        glyph,
        glyph_size,
        (icon_x + (icon_size - gw) / 2.0) as i32,
        (y + (row - glyph_size * 1.35) / 2.0) as i32,
        crate::theme::accent(),
    );
    let name_x = icon_x + icon_size + 10.0 * scale;
    text.draw(
        canvas,
        label,
        size,
        name_x as i32,
        (y + (row - size * 1.35) / 2.0) as i32,
        if highlighted {
            crate::theme::TEXT
        } else {
            crate::theme::TEXT_DIM
        },
    );
}

/// Saturation and lightness the launcher paints every icon at.
///
/// One saturation and one lightness for every application, and only the
/// hue from the artwork: that is what makes twelve different icons read as
/// one set rather than twelve logos. The lightness sits where a pastel is
/// legible on the panel's dark ground; the bottom of the gradient is a
/// little deeper, which is enough for the glyph to feel lit from above.
const TINT_SATURATION: f32 = 0.78;
const TINT_LIGHTNESS: (f32, f32) = (0.74, 0.62);

/// The file the launcher draws for `name`: the symbolic variant if the
/// theme has one, otherwise the ordinary icon.
///
/// Everything the launcher draws goes through [`tinted`], and tinting a
/// full-colour icon that sits on an opaque square background yields a
/// filled square with the glyph lost inside it. The symbolic variant is a
/// flat glyph drawn for exactly this treatment. The dock keeps the coloured
/// artwork and calls [`Icons::find`] directly, so the preference lives here
/// rather than in the lookup.
pub(crate) fn launcher_icon(
    icons: &Icons,
    name: &str,
    size: u32,
    density: u32,
) -> Option<std::path::PathBuf> {
    icons
        .find_symbolic(name, size, density)
        .or_else(|| icons.find(name, size, density))
}

/// `icon`, repainted in its own dominant hue.
///
/// An icon with no hue to speak of takes the accent, so a monochrome glyph
/// is still one of the family rather than the one grey thing in the grid.
pub(crate) fn tinted(icon: &raven_desktop::Pixmap) -> raven_desktop::Pixmap {
    let hue = icon.hue().unwrap_or_else(accent_hue);
    icon.tinted(
        hsl(hue, TINT_SATURATION, TINT_LIGHTNESS.0),
        hsl(hue, TINT_SATURATION, TINT_LIGHTNESS.1),
    )
}

/// The theme accent's hue, so a fallback tint matches the focus ring.
fn accent_hue() -> f32 {
    let [r, g, b, _] = crate::theme::accent()
        .to_rgba_bytes()
        .map(|c| f32::from(c) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let chroma = max - min;
    if chroma < 1e-3 {
        return 0.0;
    }
    let hue = if max == r {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    };
    hue * 60.0
}

/// HSL, with hue in degrees, to RGB bytes.
fn hsl(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// How long ago `secs` was, the way a person would say it.
///
/// Coarse on purpose: "2m ago" is a reminder, not a log entry, and a figure
/// that ticked every second would draw the eye to a number that means nothing.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "Just now".to_owned(),
        60..3_600 => format!("{}m ago", secs / 60),
        3_600..86_400 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// What an application *is*, for the right-hand side of a result row.
///
/// The generic name ("Web Browser") is a category and fits on a row; the
/// comment ("Browse the World Wide Web") is a sentence, and only stands in
/// when there is no generic name. Blank values are treated as absent so a
/// `GenericName=` line with nothing after it draws nothing.
pub(crate) fn kind_of(entry: &Entry) -> Option<&str> {
    entry
        .generic_name
        .as_deref()
        .or(entry.comment.as_deref())
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
}

/// `name`, cut with an ellipsis to fit `max_w` pixels at `size`.
///
/// A tile is a label, not a document: a long name is truncated rather than
/// wrapped, so the grid stays a grid.
pub(crate) fn fit(text: &mut Text, name: &str, size: f32, max_w: f32) -> String {
    if text.measure(name, size).0 <= max_w {
        return name.to_owned();
    }
    // The longest cut that fits, found by halving: a measurement shapes the
    // string, and a file name can be sixty characters that would otherwise
    // be shaped sixty times per row per keystroke.
    let chars: Vec<char> = name.chars().collect();
    let cut = |keep: usize| {
        let head: String = chars[..keep].iter().collect();
        format!("{}…", head.trim_end())
    };
    let (mut fits, mut top) = (0, chars.len().saturating_sub(1));
    while fits < top {
        let mid = top - (top - fits) / 2;
        if text.measure(&cut(mid), size).0 <= max_w {
            fits = mid;
        } else {
            top = mid - 1;
        }
    }
    cut(fits)
}

#[cfg(test)]
mod render_tests {
    use super::tests::{CLOCK, NOW, STILL, apps, entry};
    use super::*;

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
        )
        .0;
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
        assert!(
            small.w() < big.w() && small.h() < big.h(),
            "it did not grow"
        );
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
        assert_eq!(
            launcher.query(),
            "firefox",
            "the query was cleared mid-dismissal"
        );
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
        // The corners are rounded and so transparent by design; everything
        // inside the radius must be solid.
        let (canvas, _) = drawn("", 1);
        let inset = RADIUS as usize + 1;
        let mut holes = 0;
        for row in inset..canvas.height - inset {
            for col in inset..canvas.stride - inset {
                if canvas.pixels[(row * canvas.stride + col) * 4 + 3] < 0x80 {
                    holes += 1;
                }
            }
        }
        assert_eq!(holes, 0, "{holes} near-transparent pixels in the panel");
    }

    #[test]
    fn the_highlighted_row_is_tinted_rather_than_flooded() {
        // A solid accent row is unreadable and was what the bug produced.
        let (canvas, _) = drawn("", 0);
        let accent = crate::theme::accent().to_rgba_bytes();
        let flooded = canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] == accent[0] && p[1] == accent[1] && p[2] == accent[2])
            .count();
        // The caret and the selection ring are solid accent; a flooded tile
        // would be tens of thousands of pixels.
        assert!(
            flooded < 4_000,
            "{flooded} solid-accent pixels; the row is flooded"
        );
    }

    #[test]
    fn the_panel_grows_and_shrinks_with_the_result_count() {
        let (many, _) = drawn("", 0);
        let (few, _) = drawn("raven", 0);
        assert!(
            few.height < many.height,
            "the panel did not shrink to its results"
        );
    }

    /// `compose()` over `apps`, with `query` typed.
    fn composed(apps: &[Entry], query: &str) -> Canvas {
        let mut text = Text::new();
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        launcher.open(apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), apps, &frecency, NOW, CLOCK, STILL);
        }
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        compose(
            &launcher,
            apps,
            &mut text,
            &icons,
            &mut pixmaps,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        )
        .0
    }

    /// Pixels lit well above the panel's ground: glyphs, icons, the caret.
    /// The panel and its wells are near-black; anything readable is not.
    fn ink(canvas: &Canvas) -> usize {
        canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 0x50 || p[1] > 0x50 || p[2] > 0x50)
            .count()
    }

    #[test]
    fn a_list_row_says_what_the_application_is() {
        // A search for "browser" lands on Firefox because its generic name
        // matched. Without "Web Browser" on the row the hit looks like a
        // mistake, so the kind is drawn — visibly.
        let mut without = apps();
        let mut with = apps();
        with[0].generic_name = Some("Web Browser".to_owned());
        without[0].generic_name = None;
        let plain = composed(&without, "firefox");
        let kinded = composed(&with, "firefox");
        assert!(
            ink(&kinded) > ink(&plain),
            "the generic name was not drawn: {} vs {} inked pixels",
            ink(&kinded),
            ink(&plain)
        );
    }

    #[test]
    fn the_panel_never_grows_past_a_screenful_of_applications() {
        // Twelve matches, eight rows: the panel is as tall as with eight
        // matches, and stays that tall however far the highlight scrolls.
        let twelve: Vec<Entry> = (1..=12)
            .map(|n| entry(&format!("Tool {n:02}"), &format!("/bin/tool{n}")))
            .collect();
        let eight = twelve[..VISIBLE].to_vec();
        let capped = composed(&twelve, "tool");
        let full = composed(&eight, "tool");
        assert_eq!(
            capped.height, full.height,
            "the panel grew past VISIBLE rows"
        );

        let mut text = Text::new();
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        launcher.open(&twelve, &frecency, NOW, None, CLOCK, STILL);
        for c in "tool".chars() {
            launcher.press(Key::Insert(c), &twelve, &frecency, NOW, CLOCK, STILL);
        }
        for _ in 0..11 {
            launcher.press(Key::Down, &twelve, &frecency, NOW, CLOCK, STILL);
        }
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let scrolled = compose(
            &launcher,
            &twelve,
            &mut text,
            &icons,
            &mut pixmaps,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        )
        .0;
        assert_eq!(scrolled.height, full.height, "scrolling changed the height");
    }

    #[test]
    fn the_kind_does_not_change_the_height_of_the_row() {
        // It sits beside the name, not under it: the panel stays the size
        // it was.
        let mut with = apps();
        with[0].generic_name = Some("Web Browser".to_owned());
        let plain = composed(&apps(), "firefox");
        let kinded = composed(&with, "firefox");
        assert_eq!(plain.height, kinded.height, "the panel grew");
    }

    #[test]
    fn the_comment_stands_in_when_there_is_no_generic_name() {
        let mut with = apps();
        with[0].comment = Some("Browse the World Wide Web".to_owned());
        let plain = composed(&apps(), "firefox");
        let commented = composed(&with, "firefox");
        assert!(ink(&commented) > ink(&plain), "the comment was not drawn");
    }

    #[test]
    fn a_blank_generic_name_draws_nothing() {
        let mut with = apps();
        with[0].generic_name = Some("   ".to_owned());
        let plain = composed(&apps(), "firefox");
        let blank = composed(&with, "firefox");
        assert_eq!(ink(&blank), ink(&plain), "whitespace was drawn");
    }

    #[test]
    fn a_query_matching_nothing_still_draws_a_field() {
        // An empty result set must not produce a zero-height canvas.
        let (canvas, _) = drawn("qqzzxx", 0);
        assert!(canvas.height > 0 && canvas.stride > 0);
    }

    #[test]
    fn the_run_row_and_the_result_row_are_drawn() {
        // Each adds a row to the panel, and each puts ink on it: a row
        // that changed the height but drew nothing would be a blank strip.
        let (empty, _) = drawn("qqzzxx", 0);
        // "1/0" has no value, so it draws exactly what "2+2" does minus the
        // result row — same run row, and with the highlight moved down onto
        // it, the same footer.
        let (undefined, _) = drawn("1/0", 0);
        let (summed, _) = drawn("2+2", 1);
        assert!(
            summed.height > undefined.height,
            "the result row did not add to the panel"
        );
        assert!(ink(&summed) > ink(&undefined), "the result was not drawn");
        let (word, _) = drawn("fire", 0);
        // "fire" is one application row; "qqzzxx" is one run row. Same
        // number of rows, so a run row that took no space would show here.
        assert!(
            (word.height as i64 - empty.height as i64).abs() < (BASE_SIZE * 3.0) as i64,
            "the run row is not the height of a row: {} vs {}",
            empty.height,
            word.height
        );
    }

    #[test]
    fn the_footer_hints_follow_the_target() {
        let has = |hints: &[(&str, &str)], key: &str| hints.iter().any(|(k, _)| *k == key);
        // An application offers Tab; nothing else does.
        assert!(has(hints_for(Some(Target::App(0)), false, false), "Tab"));
        assert!(!has(hints_for(Some(Target::File(0)), false, false), "Tab"));
        assert!(!has(hints_for(Some(Target::Command), false, false), "Tab"));
        assert!(!has(hints_for(Some(Target::Result), false, false), "Tab"));
        // A file still opens on Enter; a result has no Enter at all.
        assert!(has(hints_for(Some(Target::File(0)), false, false), "Enter"));
        assert!(!has(hints_for(Some(Target::Result), false, false), "Enter"));
        assert_eq!(hints_for(Some(Target::File(0)), false, false), FILE_HINTS);
        assert_eq!(
            hints_for(Some(Target::Command), false, false),
            COMMAND_HINTS
        );
        // The grid says "←↑↓→" for an application, and nothing selected.
        assert_eq!(hints_for(Some(Target::App(0)), false, true), GRID_HINTS);
        assert_eq!(hints_for(None, false, true), GRID_HINTS);
        assert_eq!(hints_for(None, false, false), LIST_HINTS);
    }

    #[test]
    fn the_menu_hints_win_whatever_is_highlighted() {
        for target in [
            None,
            Some(Target::App(0)),
            Some(Target::File(0)),
            Some(Target::Command),
        ] {
            assert_eq!(hints_for(target, true, false), MENU_HINTS);
            assert_eq!(hints_for(target, true, true), MENU_HINTS);
        }
    }

    #[test]
    fn the_footer_changes_with_the_row_under_the_highlight() {
        // "Enter Run" on the run row, no Enter at all on the result row:
        // three different footers for the same query, so three different
        // amounts of ink.
        let (result, _) = drawn("2+2", 0);
        let (command, _) = drawn("2+2", 1);
        assert_ne!(
            ink(&result),
            ink(&command),
            "the footer did not change between the result and the run row"
        );
    }

    // -- The layout ---------------------------------------------------------

    fn centre(rect: Rect) -> Point {
        rect.center()
    }

    /// `compose()` over `apps`, with `query` typed and `used` recorded as
    /// launched, most recent first, so the grid has recent rows.
    fn laid_out(apps: &[Entry], query: &str, used: &[usize], tab: bool) -> (Launcher, Layout) {
        let mut text = Text::new();
        let mut frecency = Frecency::default();
        for (n, index) in used.iter().enumerate() {
            frecency.record(&apps[*index].path, NOW - 60 * n as u64);
        }
        let mut launcher = Launcher::default();
        launcher.open(apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), apps, &frecency, NOW, CLOCK, STILL);
        }
        if tab {
            launcher.press(Key::Actions, apps, &frecency, NOW, CLOCK, STILL);
        }
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let (_, layout) = compose(
            &launcher,
            apps,
            &mut text,
            &icons,
            &mut pixmaps,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        );
        (launcher, layout)
    }

    #[test]
    fn a_point_inside_a_tile_maps_to_that_tile_and_outside_to_nothing() {
        let apps = apps();
        let (launcher, layout) = laid_out(&apps, "", &[], false);
        assert!(launcher.is_grid());
        assert_eq!(layout.hits.len(), launcher.visible.len());
        for (rect, slot) in &layout.hits {
            assert_eq!(layout.hit(centre(*rect)), Some(*slot), "tile {slot}");
            // Just outside its corner is not it.
            let outside = Point::new(rect.x() - 1, rect.y() - 1);
            assert_ne!(layout.hit(outside), Some(*slot), "tile {slot} bled outward");
        }
        // The field, at the top, is not a target.
        assert_eq!(layout.hit(Point::new(layout.size.0 / 2, 30)), None);
        // Nor is anything off the canvas.
        assert_eq!(layout.hit(Point::new(-1, -1)), None);
        assert_eq!(
            layout.hit(Point::new(layout.size.0 + 1, layout.size.1 + 1)),
            None
        );
    }

    #[test]
    fn tiles_and_recent_rows_are_numbered_in_navigation_order() {
        // Eight used applications: six fill the grid, and the two the grid
        // has no room for are recent rows under it — and the pointer must
        // number them the way Down does, or hovering the first recent row
        // highlights a tile.
        let mut eight = apps();
        for name in ["Gimp", "Inkscape", "Kitty", "Vim"] {
            eight.push(entry(name, &format!("/bin/{}", name.to_lowercase())));
        }
        let used: Vec<usize> = (0..eight.len()).collect();
        let (launcher, layout) = laid_out(&eight, "", &used, false);
        assert_eq!(launcher.recent().len(), 2);
        assert_eq!(layout.hits.len(), SUGGESTED + 2);
        let mut slots: Vec<usize> = layout.hits.iter().map(|(_, s)| *s).collect();
        slots.sort_unstable();
        assert_eq!(slots, (0..SUGGESTED + 2).collect::<Vec<_>>());
        // The recent rows are below every tile, and in order.
        let tile_bottom = layout.hits[..SUGGESTED]
            .iter()
            .map(|(r, _)| r.bottom())
            .max()
            .unwrap();
        let (first, second) = (layout.hits[SUGGESTED], layout.hits[SUGGESTED + 1]);
        assert!(
            first.0.y() >= tile_bottom,
            "a recent row overlapped the tiles"
        );
        assert!(
            second.0.y() > first.0.y(),
            "the recent rows are out of order"
        );
        assert_eq!((first.1, second.1), (SUGGESTED, SUGGESTED + 1));
    }

    #[test]
    fn the_list_rows_map_to_their_places_in_the_navigation_order() {
        // "2+2": a result row, no application, and the run row — the two
        // kinds of row that are neither an application nor a file.
        let apps = apps();
        let (launcher, layout) = laid_out(&apps, "2+2", &[], false);
        assert_eq!(launcher.visible, vec![Target::Result, Target::Command]);
        assert_eq!(layout.hits.len(), 2);
        assert_eq!(layout.hit(centre(layout.hits[0].0)), Some(0));
        assert_eq!(layout.hit(centre(layout.hits[1].0)), Some(1));
        assert!(layout.hits[0].0.bottom() <= layout.hits[1].0.y());

        // And a list of applications, top to bottom in result order.
        let (launcher, layout) = laid_out(&apps, "f", &[], false);
        assert_eq!(layout.hits.len(), launcher.window().len());
        for (n, (rect, slot)) in layout.hits.iter().enumerate() {
            assert_eq!(*slot, n, "row {n} is numbered {slot}");
            assert_eq!(layout.hit(centre(*rect)), Some(n));
        }
    }

    #[test]
    fn the_menu_covers_the_rows_under_it() {
        let apps = apps();
        let (launcher, layout) = laid_out(&apps, "", &[], true);
        assert!(launcher.menu().is_some());
        // "Open" and "Pin": the fixtures have no actions.
        assert_eq!(layout.menu_hits.len(), 2);
        let item = centre(layout.menu_hits[0].0);
        assert_eq!(layout.menu_hit(item), Some(0));
        // The menu is on top, so the tile under it is not what is there.
        assert_eq!(layout.hit(item), None);
        // Without the menu, nothing is in the way.
        let (_, bare) = laid_out(&apps, "", &[], false);
        assert!(bare.menu_hits.is_empty());
        assert_eq!(bare.menu_hit(item), None);
    }

    #[test]
    fn the_menu_stays_inside_the_body_over_a_short_list() {
        // One row, and a menu taller than it: the panel grows to hold the
        // menu, so it neither covers the footer nor runs off the canvas.
        let mut apps = apps();
        apps[0].actions.push(raven_desktop::entry::Action {
            id: "private".into(),
            name: "Private Window".into(),
            exec: "firefox --private-window".into(),
            icon: None,
        });
        let query: String = apps[0].name.chars().take(4).collect();
        let (launcher, layout) = laid_out(&apps, &query, &[], true);
        assert_eq!(
            launcher.window().len(),
            1,
            "the fixture did not narrow to one row"
        );
        assert_eq!(layout.menu_hits.len(), 3, "Open, the action, and Pin");
        let (_, bare) = laid_out(&apps, &query, &[], false);
        assert!(
            bare.size.1 < layout.size.1,
            "the panel did not grow for the menu"
        );
        let scale = 1.0;
        let size = BASE_SIZE * scale;
        let footer_top = layout.size.1 as f32 - PAD * scale - size * 2.4;
        for (rect, item) in &layout.menu_hits {
            assert!(
                rect.bottom() <= footer_top as i32,
                "menu item {item} at {rect:?} reaches the footer starting at {footer_top}"
            );
            assert_eq!(layout.menu_hit(centre(*rect)), Some(*item));
        }
    }

    #[test]
    fn the_pointer_is_scaled_by_where_the_panel_was_placed() {
        // Half open, the 560×360 canvas is drawn into a 280×180 rectangle at
        // (100, 100): a pointer at its far corner is the canvas's far corner.
        let layout = Layout {
            size: (560, 360),
            ..Layout::default()
        };
        let panel = Rect::from_xywh(100, 100, 280, 180);
        assert_eq!(
            layout.canvas_point(panel, Point::new(240, 190)),
            Some(Point::new(280, 180))
        );
        assert_eq!(
            layout.canvas_point(panel, Point::new(100, 100)),
            Some(Point::new(0, 0))
        );
        // Off the panel is off the panel.
        assert_eq!(layout.canvas_point(panel, Point::new(99, 100)), None);
        assert_eq!(layout.canvas_point(panel, Point::new(380, 280)), None);
        // At full size on a 1× output, a canvas pixel is a logical one.
        let full = Rect::from_xywh(680, 360, 560, 360);
        assert_eq!(
            layout.canvas_point(full, Point::new(700, 400)),
            Some(Point::new(20, 40))
        );
    }

    /// Dump the launcher to a PPM so it can be looked at.
    ///
    /// `LAUNCHER_DUMP=/tmp/l.ppm LAUNCHER_QUERY=fi cargo test -p huginn-comp launcher_dump`
    ///
    /// `LAUNCHER_USED=Firefox,Vim,Kitty` marks those as launched, most
    /// recent first, so the recent rows have something to show.
    ///
    /// `LAUNCHER_QUERY=browser` is a good look at the kind column: the rows
    /// that matched on their generic name or comment say so at the right.
    ///
    /// `LAUNCHER_QUERY=2+2` shows the result row and, under it, the run row;
    /// `LAUNCHER_QUERY="htop -d 5"` the run row alone.
    #[test]
    fn launcher_dump() {
        let Ok(path) = std::env::var("LAUNCHER_DUMP") else {
            return;
        };
        let mut text = Text::new();
        let apps = scan_applications();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut frecency = Frecency::default();
        for (n, name) in std::env::var("LAUNCHER_USED")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .enumerate()
        {
            if let Some(app) = apps.iter().find(|a| a.name.eq_ignore_ascii_case(name)) {
                frecency.record(&app.path, now - 90 * n as u64);
            }
        }
        let mut launcher = Launcher::default();
        // `LAUNCHER_FILES=1`: index the real home, so file rows have
        // something to show.
        if std::env::var_os("LAUNCHER_FILES").is_some()
            && let Some(home) = std::env::var_os("HOME")
        {
            launcher.set_files(std::sync::Arc::new(FileIndex::build(
                std::path::Path::new(&home),
                raven_desktop::files::Limits::default(),
            )));
        }
        launcher.open(&apps, &frecency, now, None, CLOCK, STILL);
        for c in std::env::var("LAUNCHER_QUERY").unwrap_or_default().chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, now, CLOCK, STILL);
        }
        for _ in 0..std::env::var("LAUNCHER_DOWN")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
        {
            launcher.press(Key::Down, &apps, &frecency, now, CLOCK, STILL);
        }
        // `LAUNCHER_TAB=2`: open the actions menu and move down twice.
        if let Some(steps) = std::env::var("LAUNCHER_TAB")
            .ok()
            .and_then(|d| d.parse::<usize>().ok())
        {
            launcher.press(Key::Actions, &apps, &frecency, now, CLOCK, STILL);
            for _ in 0..steps {
                launcher.press(Key::Down, &apps, &frecency, now, CLOCK, STILL);
            }
        }

        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        let (canvas, layout) =
            compose(&launcher, &apps, &mut text, &icons, &mut pixmaps, output, 1);
        // Where the pointer would land, to check against the picture.
        for (rect, slot) in &layout.hits {
            println!("hit {slot}: {rect:?}");
        }
        for (rect, item) in &layout.menu_hits {
            println!("menu {item}: {rect:?}");
        }

        let mut ppm = format!("P6\n{} {}\n255\n", canvas.stride, canvas.height).into_bytes();
        for pixel in canvas.pixels.as_chunks::<4>().0.iter() {
            ppm.extend_from_slice(&pixel[..3]);
        }
        std::fs::write(&path, ppm).expect("writing the dump");
        println!("wrote {}x{} to {path}", canvas.stride, canvas.height);
    }

    /// `LAUNCHER_TIME=1 [LAUNCHER_QUERY=word] [LAUNCHER_DENSITY=2] cargo test
    /// --release -p huginn-comp launcher_time -- --nocapture`: type the
    /// query one character at a time against the real applications and the
    /// real home index and print what each stage of a keystroke costs.
    #[test]
    fn launcher_time() {
        if std::env::var_os("LAUNCHER_TIME").is_none() {
            return;
        }
        let t = std::time::Instant::now();
        let mut text = Text::new();
        let apps = scan_applications();
        println!("scan_applications: {:?} ({} apps)", t.elapsed(), apps.len());
        let now = 0;
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        if let Some(home) = std::env::var_os("HOME") {
            let t = std::time::Instant::now();
            let index = FileIndex::build(
                std::path::Path::new(&home),
                raven_desktop::files::Limits::default(),
            );
            println!(
                "FileIndex::build: {:?} ({} files)",
                t.elapsed(),
                index.len()
            );
            launcher.set_files(std::sync::Arc::new(index));
        }
        let density: u32 = std::env::var("LAUNCHER_DENSITY")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(1);
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let icons = Icons::discover(
            &std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| crate::theme::ICON_THEME.into()),
        );
        let mut pixmaps = Pixmaps::new();
        launcher.open(&apps, &frecency, now, None, CLOCK, STILL);
        let query = std::env::var("LAUNCHER_QUERY").unwrap_or_else(|_| "cargo lock".into());
        // Micro-timings of the pieces a row is drawn from.
        {
            let m = Metrics::for_output(output, density);
            let icon_size = (m.size * 2.4) as u32;
            for entry in apps.iter().take(6) {
                let Some(name) = entry.icon.as_deref() else {
                    continue;
                };
                let t = std::time::Instant::now();
                let path = launcher_icon(&icons, name, icon_size / density, density);
                let lookup = t.elapsed();
                let t = std::time::Instant::now();
                let pixmap = path.as_deref().and_then(|p| pixmaps.get(p, icon_size));
                let get = t.elapsed();
                let t = std::time::Instant::now();
                let _ = pixmap.map(tinted);
                let tint = t.elapsed();
                println!(
                    "icon {name:>28}: lookup {lookup:>9?} | pixmaps.get {get:>9?} | tinted {tint:>9?} -> {path:?}"
                );
            }
            let mut canvas = Canvas::new(m.width, 600);
            let t = std::time::Instant::now();
            text.draw(
                &mut canvas,
                "Raven Terminal",
                m.size,
                10,
                10,
                crate::theme::TEXT,
            );
            println!("text.draw: {:?}", t.elapsed());
            let t = std::time::Instant::now();
            let _ = text.measure("Raven Terminal", m.size);
            println!("text.measure: {:?}", t.elapsed());
            let t = std::time::Instant::now();
            let _ = fit(
                &mut text,
                "A very long application name that will not fit at all",
                m.size,
                100.0,
            );
            println!("fit(long): {:?}", t.elapsed());
            let t = std::time::Instant::now();
            canvas.fill_rounded(
                0,
                0,
                m.width,
                600,
                m.radius,
                crate::theme::BACKGROUND.with_alpha(ALPHA),
            );
            println!("fill_rounded {}x600: {:?}", m.width, t.elapsed());
        }
        for c in query.chars() {
            let t = std::time::Instant::now();
            launcher.press(Key::Insert(c), &apps, &frecency, now, CLOCK, STILL);
            let pressed = t.elapsed();
            let t = std::time::Instant::now();
            let apps_only = search(&apps, launcher.query(), &frecency, now).len();
            let app_search = t.elapsed();
            let t = std::time::Instant::now();
            let files = launcher.files().search(launcher.query(), FILES).len();
            let file_search = t.elapsed();
            let t = std::time::Instant::now();
            let _ = compose(
                &launcher,
                &apps,
                &mut text,
                &icons,
                &mut pixmaps,
                output,
                density,
            );
            let composed = t.elapsed();
            let t = std::time::Instant::now();
            let (canvas, _) = compose(
                &launcher,
                &apps,
                &mut text,
                &icons,
                &mut pixmaps,
                output,
                density,
            );
            let composed_again = t.elapsed();
            let t = std::time::Instant::now();
            let _ = Panel::from_canvas(&canvas, density);
            let panel = t.elapsed();
            println!(
                "{:>12?}  press {:>9?} | app search {:>9?} ({apps_only}) | file search {:>9?} ({files}) | compose {:>9?} / again {:>9?} | panel {:>9?}",
                launcher.query(),
                pressed,
                app_search,
                file_search,
                composed,
                composed_again,
                panel
            );
        }
    }
}

#[cfg(test)]
mod blur_tests {
    use super::*;

    #[test]
    fn the_blur_sits_inside_the_panel_by_the_corner_radius() {
        // The panel's corners are transparent, so a blur that reached them
        // would show as four square corners around a rounded panel. Inset by
        // exactly the radius, the blur stays under the opaque part.
        let panel = Rect::from_xywh(100, 200, 640, 480);
        let blur = blur_rect(panel).expect("a full-size panel blurs");
        let radius = RADIUS as i32;
        assert_eq!(blur.x(), panel.x() + radius);
        assert_eq!(blur.y(), panel.y() + radius);
        assert_eq!(blur.right(), panel.right() - radius);
        assert_eq!(blur.bottom(), panel.bottom() - radius);
    }

    #[test]
    fn a_panel_too_small_for_the_inset_does_not_blur() {
        // At the start of the reveal the panel is scaled down; a rectangle
        // inset past its own edges is inverted, and the renderer must be told
        // "no blur" rather than handed a negative crop.
        let radius = RADIUS as i32;
        assert_eq!(blur_rect(Rect::from_xywh(0, 0, radius * 2, 300)), None);
        assert_eq!(blur_rect(Rect::from_xywh(0, 0, 300, radius * 2)), None);
        assert_eq!(blur_rect(Rect::ZERO), None);
        assert!(blur_rect(Rect::from_xywh(0, 0, radius * 2 + 1, radius * 2 + 1)).is_some());
    }

    #[test]
    fn the_blur_follows_the_panel_through_the_reveal() {
        // The placement moves and grows as the panel arrives from the dock,
        // and the blur is cut from the placement, so it moves and grows with
        // it: a blur parked at the panel's final rectangle while the panel was
        // still arriving would blur a patch of desktop with nothing over it.
        let output = Rect::from_xywh(0, 0, 1920, 1080);
        let origin = Some(Rect::from_xywh(20, 1030, 44, 44));
        let half = placement(output, (700, 500), origin, 0.5);
        let full = placement(output, (700, 500), origin, 1.0);
        let (half_blur, full_blur) = (blur_rect(half).unwrap(), blur_rect(full).unwrap());
        assert!(half_blur.w() < full_blur.w());
        assert_ne!(half_blur.center(), full_blur.center());
        // And is always strictly within the panel it belongs to.
        for (panel, blur) in [(half, half_blur), (full, full_blur)] {
            assert!(blur.x() > panel.x() && blur.right() < panel.right());
            assert!(blur.y() > panel.y() && blur.bottom() < panel.bottom());
        }
    }
}
