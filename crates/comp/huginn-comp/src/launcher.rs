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

mod arc;
mod list;
mod paint;

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
    /// A page of the arc at a time, or three rows of the list's grid.
    PageUp,
    PageDown,
    /// `Ctrl`+`Left`/`Right`: the previous or next filter while searching —
    /// All, Apps, Files — or, on the arc before anything is typed, the
    /// previous or next category.
    PrevGroup,
    NextGroup,
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
            keysyms::KEY_Left if ctrl => Self::PrevGroup,
            keysyms::KEY_Right if ctrl => Self::NextGroup,
            keysyms::KEY_Page_Up => Self::PageUp,
            keysyms::KEY_Page_Down => Self::PageDown,
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
    /// Pin `entry` to the pin bar, or unpin it if it already is. The
    /// launcher stays open: pinning is bookkeeping, not a launch, and the
    /// user may well want to pin two things in a row. The compositor owns
    /// the pin list (see [`crate::pins`]) and tells the launcher what it
    /// now holds through [`Launcher::set_pinned`].
    TogglePin { entry: std::path::PathBuf },
    /// Close the launcher and open the pin bar in its place: the
    /// list's "Pin bar" link, for the pins that did not fit the foot.
    OpenPinned,
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

/// How the launcher is laid out. Chosen in quick settings, and by
/// `appearance.launcher_layout` in `desktop.toml`.
///
/// Two layouts over one launcher rather than two launchers: the query, the
/// ranking, the actions menu and pinning are the same whichever is showing,
/// and only how they are arranged — and so what the arrow keys walk — differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Style {
    /// A search bar that opens into a grid: suggestions with the pinned and
    /// recent applications along the foot before anything is typed, results
    /// once something is. The default, because a list of names is what
    /// everyone already knows how to read.
    #[default]
    List,
    /// Seven applications on an arc around the search, the categories beside
    /// it, and the highlighted application's details and actions on the other
    /// side.
    Arc,
}

impl Style {
    pub(crate) const ALL: [Self; 2] = [Self::List, Self::Arc];

    /// What the quick settings row shows, and what `desktop.toml` says.
    pub(crate) fn value(self) -> &'static str {
        match self {
            Self::List => "List",
            Self::Arc => "Arc",
        }
    }

    pub(crate) fn from_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.value().eq_ignore_ascii_case(value.trim()))
    }

    /// The next layout, `delta` steps along [`Self::ALL`], wrapping.
    pub(crate) fn stepped(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|s| *s == self).unwrap_or(0) as i32;
        Self::ALL[(at + delta).rem_euclid(n) as usize]
    }
}

/// Which kinds of result a search shows.
///
/// Tabs in the list, the sidebar on the arc. Counted before filtering, so
/// the tab says how many files a search found while the apps are showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Filter {
    #[default]
    All,
    Apps,
    Files,
}

impl Filter {
    pub(crate) const ALL: [Self; 3] = [Self::All, Self::Apps, Self::Files];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Apps => "Apps",
            Self::Files => "Files",
        }
    }

    fn stepped(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|f| *f == self).unwrap_or(0) as i32;
        Self::ALL[(at + delta).rem_euclid(n) as usize]
    }
}

/// How a search's results are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Sort {
    /// Match quality, then frecency: what [`search`] returns.
    #[default]
    Relevance,
    /// Alphabetical, for someone scanning for a name they half remember.
    Name,
}

impl Sort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Relevance => "Best match",
            Self::Name => "Name",
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Relevance => Self::Name,
            Self::Name => Self::Relevance,
        }
    }
}

/// What the arc's sidebar groups applications by, before anything is typed.
///
/// A handful of groups over the freedesktop `Categories` rather than the
/// spec's dozens: a sidebar of "AudioVideo", "Audio" and "Video" as three
/// rows is three chances to look in the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    All,
    Development,
    Media,
    Internet,
    Utilities,
    System,
}

impl Category {
    pub(crate) const ALL: [Self; 6] = [
        Self::All,
        Self::Development,
        Self::Media,
        Self::Internet,
        Self::Utilities,
        Self::System,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "All Apps",
            Self::Development => "Development",
            Self::Media => "Media",
            Self::Internet => "Internet",
            Self::Utilities => "Utilities",
            Self::System => "System",
        }
    }

    /// Whether `entry` belongs here, by the categories its desktop file lists.
    pub(crate) fn contains(self, entry: &Entry) -> bool {
        let has = |names: &[&str]| {
            entry
                .categories
                .iter()
                .any(|c| names.iter().any(|n| c.eq_ignore_ascii_case(n)))
        };
        match self {
            Self::All => true,
            Self::Development => has(&["Development"]),
            Self::Media => has(&["AudioVideo", "Audio", "Video", "Graphics"]),
            Self::Internet => has(&["Network"]),
            Self::Utilities => has(&["Utility"]),
            Self::System => has(&["System", "Settings"]),
        }
    }
}

/// Something on the panel that answers a click but is not in the navigation
/// order: a tab, the sort, the list's chevron, a category, a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Button {
    /// The list's chevron: open the bar into the grid, or close it again.
    Expand,
    /// Toggle between suggestions and every installed application.
    AllApps,
    /// Move through the app grid with the pointer.
    Page(isize),
    Filter(Filter),
    Sort,
    /// A category on the arc's sidebar, as an index into
    /// [`Launcher::categories`].
    Category(usize),
    /// The list's link to the pin bar.
    PinnedPanel,
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
    reveal: crate::anim::Reveal,
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
    /// Which layout draws it; see [`Style`].
    style: Style,
    /// Whether the list's grid is showing. The list opens as a bar — only
    /// the search field — and grows into the grid on Down, Tab or Return,
    /// or as soon as anything is typed. Meaningless on the arc.
    expanded: bool,
    /// Browse every installed app in the list grid.
    all_apps: bool,
    /// Which kinds of result a search shows.
    filter: Filter,
    /// How a search's results are ordered.
    sort: Sort,
    /// The categories the arc offers — [`Category::All`] and every group at
    /// least one installed application is in — and which is chosen.
    categories: Vec<Category>,
    category: usize,
    /// The list's suggestion tiles before anything is typed: the most-used
    /// applications that are not already pinned, as indices.
    suggested: Vec<usize>,
    /// The pinned applications the list's foot shows, as indices, in pin
    /// order. Those whose desktop file is not installed are left out.
    pins_shown: Vec<usize>,
    /// When each application was last launched, for "Opened 2h ago".
    last_used: Vec<(usize, u64)>,
    /// How many applications and files the search found before [`Filter`]
    /// took any away, so the tabs can say.
    found: (usize, usize),
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
            reveal: crate::anim::Reveal::hidden(),
            origin: None,
            layout: Layout::default(),
            pinned: Vec::new(),
            style: Style::default(),
            expanded: false,
            all_apps: false,
            filter: Filter::default(),
            sort: Sort::default(),
            categories: vec![Category::All],
            category: 0,
            suggested: Vec::new(),
            pins_shown: Vec::new(),
            last_used: Vec::new(),
            found: (0, 0),
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

    /// The first row of the list's result grid that is drawn; at most
    /// [`GRID_ROWS`] rows are.
    ///
    /// Scroll state, kept rather than derived from the selection, because the
    /// keyboard and the mouse want different things of it. The highlight
    /// walking down past the last drawn row pulls the grid down a row, and
    /// walking back up past the first pulls it back (see
    /// [`Self::scroll_to_selection`]). The pointer only ever lands on a tile
    /// already drawn — and a grid that re-derived itself around the hovered
    /// tile would slide the tiles out from under the pointer, so the next
    /// motion event highlighted something other than what the hand is over.
    pub(crate) fn first_row(&self) -> usize {
        self.first
    }

    /// Slide the grid the least that puts the highlight among the drawn rows.
    /// Called after the keyboard moves the highlight and after the results
    /// change; deliberately not after a hover, see [`Self::first_row`].
    fn scroll_to_selection(&mut self) {
        let tiles = self.tile_range();
        let last_first = tiles.len().div_ceil(COLUMNS).saturating_sub(GRID_ROWS);
        if self.style == Style::List && tiles.contains(&self.selected) {
            let row = (self.selected - tiles.start) / COLUMNS;
            self.first = self.first.clamp(row.saturating_sub(GRID_ROWS - 1), row);
        }
        self.first = self.first.min(last_first);
    }

    /// The tiles of the list's grid, as positions in the navigation order:
    /// the suggestions before anything is typed, and once something is, the
    /// applications and then the files — between the result row, when
    /// there is one, and the run row.
    fn tile_range(&self) -> std::ops::Range<usize> {
        if self.is_grid() {
            0..self.suggested.len()
        } else {
            let start = usize::from(self.visible.first() == Some(&Target::Result));
            start..start + self.results.len() + self.file_hits.len()
        }
    }

    /// The list's foot before anything is typed — the pinned applications,
    /// then the recent ones — as positions in the navigation order.
    fn strip_range(&self) -> std::ops::Range<usize> {
        if self.is_grid() && self.style == Style::List {
            self.suggested.len()..self.visible.len()
        } else {
            0..0
        }
    }

    /// Which layout draws the launcher.
    pub(crate) fn style(&self) -> Style {
        self.style
    }

    /// Lay out in `style` from now on. Returns whether that was a change.
    ///
    /// The navigation order differs between the layouts, so the caller
    /// re-ranks an open launcher afterwards ([`Self::reindex`]); the
    /// highlight goes back to the top rather than to whatever sits at the
    /// same position in a different arrangement.
    pub(crate) fn set_style(&mut self, style: Style) -> bool {
        if self.style == style {
            return false;
        }
        self.style = style;
        self.selected = 0;
        self.first = 0;
        self.menu = None;
        true
    }

    /// Whether the list is only its search bar: nothing typed, and not
    /// opened into the grid.
    pub(crate) fn is_collapsed(&self) -> bool {
        self.style == Style::List && !self.expanded && self.query.is_empty()
    }

    pub(crate) fn filter(&self) -> Filter {
        self.filter
    }

    pub(crate) fn sort(&self) -> Sort {
        self.sort
    }

    /// How many applications and files the search found, before the filter.
    pub(crate) fn found(&self) -> (usize, usize) {
        self.found
    }

    /// The categories on the arc's sidebar, and which is chosen.
    pub(crate) fn categories(&self) -> &[Category] {
        &self.categories
    }

    pub(crate) fn category(&self) -> usize {
        self.category
    }

    /// The list's suggestion tiles, as indices into the application list.
    pub(crate) fn suggested(&self) -> &[usize] {
        &self.suggested
    }

    /// The pinned applications along the list's foot, as indices.
    pub(crate) fn pins_shown(&self) -> &[usize] {
        &self.pins_shown
    }

    /// When the application at `index` was last launched, in unix seconds.
    pub(crate) fn last_used(&self, index: usize) -> Option<u64> {
        self.last_used
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, at)| *at)
    }

    /// Everything the highlight can be on, in navigation order.
    pub(crate) fn visible(&self) -> &[Target] {
        &self.visible
    }

    /// The highlight's position in [`Self::visible`].
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// Where the blurred desktop shows through the launcher at `placement`.
    ///
    /// The list is a rounded rectangle and blurs as every panel does
    /// ([`blur_rect`]). The arc is not a rectangle, and the blur path can only
    /// crop to one: the arc's composition names a rectangle that lies wholly
    /// inside its glass ([`Layout::blur`]), mapped here onto the output.
    pub(crate) fn blur_region(&self, placement: Rect) -> Option<Rect> {
        match self.layout.blur {
            Some(inner) => {
                let region = self.layout.to_output(placement, inner);
                (!region.is_empty()).then_some(region)
            }
            None => blur_rect(placement),
        }
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
        // Counted before the filter: "Files" hiding the applications that
        // matched does not make the query a command.
        !self.query.is_empty() && self.found.0 == 0
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

    /// Whether `entry` is on the pin bar, as last told.
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
        self.first = 0;
        self.menu = None;
        // Every opening starts from the same place: the bar, every result
        // kind, best match first, and all the applications on the arc.
        self.expanded = false;
        self.all_apps = false;
        self.filter = Filter::All;
        self.sort = Sort::Relevance;
        self.category = 0;
        self.origin = origin;
        self.refresh(entries, frecency, now, Keep::Top);
        self.reveal.open(clock, motion.is_reduced());
    }

    /// Dismiss it, reversing the motion it arrived with. §4.
    pub(crate) fn close(&mut self, clock: std::time::Duration, motion: crate::settings::Motion) {
        self.open = false;
        self.reveal.close(clock, motion.is_reduced());
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
                Key::Left
                | Key::Right
                | Key::PageUp
                | Key::PageDown
                | Key::PrevGroup
                | Key::NextGroup
                | Key::Ignored => return Outcome::Unchanged,
                Key::Launch => return self.launch(entries, clock, motion),
                Key::Insert(_) | Key::Backspace | Key::DeleteWord | Key::Clear => {
                    self.menu = None;
                }
            }
        }
        // The list's bar is only a field: the keys that would walk a grid
        // open it instead, and the rest have nothing to walk.
        if self.is_collapsed() {
            match key {
                Key::Down | Key::Actions | Key::Launch => {
                    self.expanded = true;
                    self.selected = 0;
                    self.first = 0;
                    return Outcome::Redraw;
                }
                Key::Up
                | Key::Left
                | Key::Right
                | Key::PageUp
                | Key::PageDown
                | Key::PrevGroup
                | Key::NextGroup => return Outcome::Unchanged,
                _ => {}
            }
        }
        match key {
            // Escape folds an opened list back into its bar before it closes
            // anything: the grid was asked for with a key, and one key puts
            // it away again. Anything typed, or the arc, closes at once.
            Key::Dismiss if self.style == Style::List && self.expanded && self.query.is_empty() => {
                self.expanded = false;
                self.selected = 0;
                self.first = 0;
                Outcome::Redraw
            }
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
            Key::Up => self.step(huginn_core::geometry::Dir::Up),
            Key::Down => self.step(huginn_core::geometry::Dir::Down),
            Key::Left => self.step(huginn_core::geometry::Dir::Left),
            Key::Right => self.step(huginn_core::geometry::Dir::Right),
            Key::PageUp => self.page(-1),
            Key::PageDown => self.page(1),
            Key::PrevGroup => self.change_group(-1, entries, frecency, now),
            Key::NextGroup => self.change_group(1, entries, frecency, now),
            Key::Insert(c) => {
                self.query.push(c);
                self.expanded = true;
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
        frecency: &Frecency,
        now: u64,
        clock: std::time::Duration,
        motion: crate::settings::Motion,
    ) -> Outcome {
        if !self.open {
            return Outcome::Unchanged;
        }
        if let Some(button) = self.layout.button(point) {
            return self.press_button(button, entries, frecency, now);
        }
        // The arc lists the highlighted application's actions all the
        // time, in its card, so one of them is a click away without Tab.
        if self.menu.is_some() || self.style == Style::Arc {
            match self.layout.menu_hit(point) {
                Some(item) => {
                    self.menu = Some(item);
                    return self.launch(entries, clock, motion);
                }
                None if self.menu.is_some() => {
                    self.menu = None;
                    return Outcome::Redraw;
                }
                None => {}
            }
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
        self.first = 0;
        // A filter is a way of looking at a search; with the search gone,
        // the next one starts from everything again.
        if self.query.is_empty() {
            self.filter = Filter::All;
        }
        self.refresh(entries, frecency, now, Keep::Top);
        Outcome::Redraw
    }

    /// A click on something that is not a result: a tab, the sort, the
    /// chevron, a category, the link to the pin bar.
    fn press_button(
        &mut self,
        button: Button,
        entries: &[Entry],
        frecency: &Frecency,
        now: u64,
    ) -> Outcome {
        match button {
            Button::Expand if self.is_collapsed() => {
                self.expanded = true;
            }
            // The chevron on an open list folds it back to the bar, taking
            // any query with it: a bar with a query in it is not a bar.
            Button::Expand => {
                self.query.clear();
                self.expanded = false;
                self.filter = Filter::All;
            }
            Button::Filter(filter) if filter == self.filter => return Outcome::Unchanged,
            Button::Filter(filter) => self.filter = filter,
            Button::Page(direction) => return self.page(direction),
            Button::AllApps => {
                self.all_apps = !self.all_apps;
                self.expanded = true;
            }
            Button::Sort => self.sort = self.sort.toggled(),
            Button::Category(index) if index == self.category || index >= self.categories.len() => {
                return Outcome::Unchanged;
            }
            Button::Category(index) => self.category = index,
            Button::PinnedPanel => return Outcome::OpenPinned,
        }
        self.selected = 0;
        self.first = 0;
        self.menu = None;
        self.refresh(entries, frecency, now, Keep::Top);
        Outcome::Redraw
    }

    /// `Ctrl`+`Left`/`Right`: the next filter while searching, or the next
    /// category on the arc before anything is typed.
    fn change_group(
        &mut self,
        delta: i32,
        entries: &[Entry],
        frecency: &Frecency,
        now: u64,
    ) -> Outcome {
        if !self.query.is_empty() {
            self.filter = self.filter.stepped(delta);
        } else if self.style == Style::List {
            self.all_apps = !self.all_apps;
        } else if self.style == Style::Arc && self.categories.len() > 1 {
            let n = self.categories.len() as i32;
            self.category = (self.category as i32 + delta).rem_euclid(n) as usize;
        } else {
            return Outcome::Unchanged;
        }
        self.selected = 0;
        self.first = 0;
        self.menu = None;
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
        self.suggested.clear();
        self.pins_shown.clear();
        self.last_used = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| frecency.last_used(&e.path).map(|t| (i, t)))
            .collect();
        // A category with nothing in it is a row that leads nowhere.
        self.categories = Category::ALL
            .into_iter()
            .filter(|c| *c == Category::All || entries.iter().any(|e| c.contains(e)))
            .collect();
        self.category = self.category.min(self.categories.len() - 1);
        if self.is_grid() {
            self.file_hits.clear();
            self.result = None;
            self.found = (self.results.len(), 0);
            match self.style {
                Style::List => {
                    // Pinned first, and not suggested again: a tile that
                    // repeats something on the foot a few pixels down tells
                    // the user nothing they were not just looking at.
                    self.pins_shown = self
                        .pinned
                        .iter()
                        .filter_map(|p| entries.iter().position(|e| e.path == *p))
                        .take(PINS_SHOWN)
                        .collect();
                    if self.all_apps && self.sort == Sort::Name {
                        self.results
                            .sort_by_cached_key(|i| entries[*i].name.to_lowercase());
                    }
                    let pins = &self.pins_shown;
                    self.suggested = self
                        .results
                        .iter()
                        .copied()
                        .filter(|i| self.all_apps || !pins.contains(i))
                        .take(if self.all_apps { usize::MAX } else { SUGGESTED })
                        .collect();
                    let suggested = &self.suggested;
                    self.recent = self
                        .last_used
                        .iter()
                        .copied()
                        .filter(|(i, _)| {
                            (self.all_apps || !suggested.contains(i)) && !pins.contains(i)
                        })
                        .collect();
                    self.recent
                        .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    self.recent.truncate(RECENT);
                    self.visible = self
                        .suggested
                        .iter()
                        .chain(&self.pins_shown)
                        .chain(self.recent.iter().map(|(i, _)| i))
                        .map(|i| Target::App(*i))
                        .collect();
                }
                Style::Arc => {
                    // Already by frecency, then name: the first page of the
                    // arc is what gets used, and the rest reads A to Z.
                    let category = self.categories[self.category];
                    self.results
                        .retain(|i| entries.get(*i).is_some_and(|e| category.contains(e)));
                    self.visible = self.results.iter().map(|i| Target::App(*i)).collect();
                }
            }
        } else {
            // Every match is navigable, not only the tiles that fit: the
            // grid scrolls under the highlight (see [`Self::first_row`]).
            // The files come after the last application, however many there
            // were. Not before the last term is two characters long, though:
            // one letter matches most of a large index, and this runs here,
            // on the compositor thread, on every keystroke — so `f` lists
            // Firefox and Files, and no files.
            let mut files = self.files.search(&self.query, FILES);
            self.found = (self.results.len(), files.len());
            self.result = calculate(&self.query);
            match self.filter {
                Filter::All => {}
                Filter::Apps => files.clear(),
                Filter::Files => self.results.clear(),
            }
            if self.sort == Sort::Name {
                self.results
                    .sort_by_cached_key(|i| entries.get(*i).map(|e| e.name.to_lowercase()));
                let index = &self.files;
                files.sort_by_cached_key(|i| index.get(*i).map(|f| f.name.to_lowercase()));
            }
            self.file_hits = files;
            // The arithmetic result first on the list: a query that
            // evaluates was asked for its value, and a value is read, not
            // chosen. The arc shows it in the hub instead, where there is no
            // row to put it on. The command last: it is the fallback,
            // offered after everything the desktop could find, and the
            // highlight should land on it only when there was nothing else.
            let result = self.result.is_some() && self.style == Style::List;
            self.visible = result
                .then_some(Target::Result)
                .into_iter()
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

    /// An arrow key, in whichever layout is showing.
    ///
    /// The arc is one line bent round: every arrow walks along it, `Left` and
    /// `Up` towards its left end and `Right` and `Down` towards its right,
    /// because on a curve "up" is a different direction at every slot. The
    /// list is a grid with rows above and below it.
    fn step(&mut self, dir: huginn_core::geometry::Dir) -> Outcome {
        use huginn_core::geometry::Dir;
        match (self.style, dir) {
            (Style::Arc, Dir::Left | Dir::Up) => self.arc_move(-1),
            (Style::Arc, Dir::Right | Dir::Down) => self.arc_move(1),
            (Style::List, Dir::Up) => self.move_vertically(-1),
            (Style::List, Dir::Down) => self.move_vertically(1),
            // Sideways only along a row of tiles or the foot, and never off
            // the end of one onto the other; a row has no sideways, and the
            // keys are swallowed rather than forwarded.
            (Style::List, Dir::Left | Dir::Right) => {
                let delta: isize = if dir == Dir::Left { -1 } else { 1 };
                let next = self.selected as isize + delta;
                let within = |range: std::ops::Range<usize>| {
                    range.contains(&self.selected) && next >= 0 && range.contains(&(next as usize))
                };
                if within(self.tile_range()) || within(self.strip_range()) {
                    self.move_selection(delta)
                } else {
                    Outcome::Unchanged
                }
            }
        }
    }

    /// `Up`/`Down` on the list: a row of tiles at a time, from the bottom row
    /// onto the foot or the run row, and from the top row onto the result.
    fn move_vertically(&mut self, direction: isize) -> Outcome {
        let (tiles, strip, selected) = (self.tile_range(), self.strip_range(), self.selected);
        if tiles.contains(&selected) {
            let next = selected as isize + direction * COLUMNS as isize;
            if next < tiles.start as isize {
                // Off the top row: onto the result row, if there is one.
                return if tiles.start > 0 {
                    self.select(tiles.start - 1)
                } else {
                    Outcome::Unchanged
                };
            }
            if next as usize >= tiles.end {
                let row = |at: usize| (at - tiles.start) / COLUMNS;
                if row(selected) != row(tiles.end - 1) {
                    // A short last row: onto its last tile.
                    return self.select(tiles.end - 1);
                }
                // Off the bottom row: onto the foot or the run row.
                return if tiles.end < self.visible.len() {
                    self.select(tiles.end)
                } else {
                    Outcome::Unchanged
                };
            }
            return self.select(next as usize);
        }
        if strip.contains(&selected) {
            if direction > 0 || tiles.is_empty() {
                return Outcome::Unchanged;
            }
            // Up off the foot: into the last row of tiles, at the same column
            // as far as the row reaches.
            let last_row = tiles.start + (tiles.len() - 1) / COLUMNS * COLUMNS;
            let column = (selected - strip.start).min(COLUMNS - 1);
            return self.select((last_row + column).min(tiles.end - 1));
        }
        // The result row or the run row: one step onto whatever is beside it.
        self.move_selection(direction)
    }

    /// Put the highlight on position `index`.
    fn select(&mut self, index: usize) -> Outcome {
        self.move_selection(index as isize - self.selected as isize)
    }

    /// A step along the arc, by where the slots sit rather than by rank:
    /// ranks go out from the top alternately left and right (see
    /// [`RANK_POS`]), so rank order would zigzag across it. Past either end
    /// is the neighbouring page.
    fn arc_move(&mut self, direction: isize) -> Outcome {
        let n = self.visible.len();
        if n == 0 {
            return Outcome::Unchanged;
        }
        let page = self.selected / ARC_SLOTS;
        let on_page = (n - page * ARC_SLOTS).min(ARC_SLOTS);
        let mut position = RANK_POS[self.selected - page * ARC_SLOTS] as isize + direction;
        // Skip the empty slots of a short last page.
        while (0..ARC_SLOTS as isize).contains(&position)
            && rank_at(position as usize).is_none_or(|rank| rank >= on_page)
        {
            position += direction;
        }
        let next = if position >= ARC_SLOTS as isize {
            if (page + 1) * ARC_SLOTS >= n {
                return Outcome::Unchanged;
            }
            // Onto the next page at its left end.
            let next_page = (page + 1) * ARC_SLOTS;
            let on_next = (n - next_page).min(ARC_SLOTS);
            next_page
                + (0..ARC_SLOTS)
                    .filter_map(rank_at)
                    .find(|rank| *rank < on_next)
                    .unwrap_or(0)
        } else if position < 0 {
            if page == 0 {
                return Outcome::Unchanged;
            }
            // Onto the previous page, which is full, at its right end.
            (page - 1) * ARC_SLOTS + rank_at(ARC_SLOTS - 1).unwrap_or(0)
        } else {
            page * ARC_SLOTS + rank_at(position as usize).unwrap_or(0)
        };
        self.select(next)
    }

    /// `PageUp`/`PageDown`: a page of the arc, landing on its top slot, or
    /// [`GRID_ROWS`] rows of the list's tiles.
    fn page(&mut self, direction: isize) -> Outcome {
        match self.style {
            Style::Arc => {
                let n = self.visible.len();
                if n == 0 {
                    return Outcome::Unchanged;
                }
                let (page, pages) = (self.selected / ARC_SLOTS, n.div_ceil(ARC_SLOTS));
                let next = (page as isize + direction).clamp(0, pages as isize - 1) as usize;
                if next == page {
                    return Outcome::Unchanged;
                }
                self.select(next * ARC_SLOTS)
            }
            Style::List => {
                let tiles = self.tile_range();
                if !tiles.contains(&self.selected) {
                    return Outcome::Unchanged;
                }
                let next = (self.selected as isize + direction * (COLUMNS * GRID_ROWS) as isize)
                    .clamp(tiles.start as isize, tiles.end as isize - 1);
                self.select(next as usize)
            }
        }
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
            mime_types: Vec::new(),
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

    /// Open over `apps` and open the list's bar into its grid, as Down does.
    fn expanded(apps: &[Entry], frecency: &Frecency) -> Launcher {
        let mut launcher = Launcher::default();
        launcher.open(apps, frecency, NOW, None, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Down, apps, frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        launcher
    }

    /// Open over `apps` on the arc and type `query`.
    fn on_arc(apps: &[Entry], query: &str) -> Launcher {
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.set_style(Style::Arc);
        launcher.open(apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), apps, &frecency, NOW, CLOCK, STILL);
        }
        launcher
    }

    /// `count` applications answering to "tool".
    fn tools(count: usize) -> Vec<Entry> {
        (1..=count)
            .map(|n| entry(&format!("Tool {n:02}"), &format!("/bin/tool{n}")))
            .collect()
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
        let mut launcher = expanded(&apps, &frecency);

        assert_eq!(
            launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(launcher.selection(), launcher.suggested().first().copied());

        for _ in 0..20 {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.selection(), launcher.suggested().last().copied());
        assert_eq!(
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn before_typing_the_arrows_walk_a_grid() {
        // Four suggestions on one row of six: Right steps one, and with no
        // row and no foot below, Down and Up have nowhere to go.
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = expanded(&apps, &frecency);
        assert!(launcher.is_grid());
        let order: Vec<usize> = launcher.suggested().to_vec();

        launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(order[1]));
        launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(order[0]));
        assert_eq!(
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        assert_eq!(
            launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn typing_turns_the_suggestions_into_a_grid_of_results() {
        let (mut launcher, apps) = typed("f");
        assert!(!launcher.is_grid() && !launcher.is_collapsed());
        let frecency = Frecency::new();
        let first = launcher.selection();
        assert_eq!(
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert_ne!(launcher.selection(), first, "Right did not step one tile");
        assert_eq!(
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged,
            "one row, and nothing under it"
        );
    }

    #[test]
    fn the_grid_never_selects_past_what_it_shows() {
        // More applications than tiles: the highlight stops at the last
        // tile rather than wandering onto a suggestion that is not drawn.
        let apps = tools(10);
        let frecency = Frecency::new();
        let mut launcher = expanded(&apps, &frecency);
        for _ in 0..20 {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.suggested().len(), SUGGESTED);
        assert_eq!(launcher.selection(), launcher.suggested().last().copied());
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

        let mut tiles: Vec<usize> = launcher.suggested().to_vec();
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
    fn down_from_the_tiles_lands_on_the_foot_and_up_goes_back() {
        let apps = tools(10);
        // Seven launched, six tiles: one overflows onto the foot.
        let mut frecency = Frecency::new();
        for (i, app) in apps.iter().take(7).enumerate() {
            frecency.record(&app.path, NOW - 1_000 + i as u64);
        }
        let mut launcher = expanded(&apps, &frecency);
        assert!(!launcher.recent().is_empty(), "nothing overflowed the grid");
        let first_recent = launcher.recent()[0].0;

        launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(first_recent));
        // One card on the foot: sideways has nowhere to go.
        assert_eq!(
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        // Up goes back into the tiles, in the same column.
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selection(), Some(launcher.suggested()[0]));
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
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw,
            "on the bar, Tab would open the grid rather than a menu"
        );
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
        let mut launcher = expanded(&apps, &frecency);
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
        let mut launcher = expanded(&apps, &frecency);
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
        let mut launcher = expanded(&apps, &frecency);
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
        let mut launcher = expanded(&apps, &frecency);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert!(launcher.is_open());
        assert_eq!(launcher.menu(), None);
        // A second Escape folds the grid back into the bar, and a third is
        // the usual one.
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert!(launcher.is_collapsed());
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Dismissed
        );
    }

    #[test]
    fn typing_puts_the_menu_away_and_edits_the_query() {
        let apps = browser();
        let frecency = Frecency::new();
        let mut launcher = expanded(&apps, &frecency);
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
    fn right_walks_from_the_last_application_onto_the_files() {
        let (mut launcher, apps) = with_files("fi");
        let frecency = Frecency::new();
        let tiles = launcher.results().len();
        for _ in 0..tiles {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
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
    /// Twelve applications answering to "tool": two full rows of tiles.
    fn many_tools() -> Vec<Entry> {
        tools(12)
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
    fn every_application_comes_before_the_files() {
        let (mut launcher, apps) = with_many_tools();
        let frecency = Frecency::new();
        for _ in 0..11 {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        }
        let last = *launcher.results().last().unwrap();
        assert_eq!(launcher.target(), Some(Target::App(last)));
        launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        assert!(matches!(launcher.target(), Some(Target::File(_))));
        // Two rows of applications and the file on a third: all drawn.
        assert_eq!(launcher.first_row(), 0);
    }

    #[test]
    fn all_apps_includes_pins_and_pages_then_returns_to_suggestions() {
        let apps = tools(30);
        let frecency = Frecency::new();
        let mut launcher = expanded(&apps, &frecency);
        launcher.set_pinned(vec![apps[29].path.clone()]);
        launcher.press_button(Button::AllApps, &apps, &frecency, NOW);
        assert_eq!(launcher.suggested().len(), apps.len());
        assert!(launcher.suggested().contains(&29));
        assert_eq!(launcher.pins_shown(), &[29]);
        launcher.press_button(Button::Page(1), &apps, &frecency, NOW);
        assert!(launcher.first_row() > 0);
        launcher.press_button(Button::Page(-1), &apps, &frecency, NOW);
        assert_eq!(launcher.first_row(), 0);
        launcher.press(Key::Insert('t'), &apps, &frecency, NOW, CLOCK, STILL);
        launcher.press(Key::Clear, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.suggested().len(), apps.len());
        launcher.press(Key::NextGroup, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.suggested().len(), SUGGESTED);
        assert!(!launcher.suggested().contains(&29));
    }

    #[test]
    fn the_grid_scrolls_with_the_highlight_both_ways() {
        let apps = tools(30);
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in "tool".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.first_row(), 0);
        for _ in 0..GRID_ROWS {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        // One row past the first screenful: the grid has slid one row.
        assert_eq!(launcher.first_row(), 1);
        for _ in 0..GRID_ROWS {
            launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(launcher.first_row(), 0);
        assert_eq!(launcher.selected(), 0);
    }

    #[test]
    fn a_short_grid_never_scrolls() {
        let (mut launcher, apps) = typed("fi");
        launcher.press(Key::Right, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert!(launcher.results().len() <= COLUMNS * GRID_ROWS);
        assert_eq!(launcher.first_row(), 0);
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
        let mut launcher = expanded(&apps, &frecency);
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
        let mut launcher = expanded(&apps, &frecency);
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
        launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
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

    // -- The two layouts ---------------------------------------------------

    #[test]
    fn the_list_opens_as_a_bar_and_asks_for_more_before_showing_it() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert!(launcher.is_collapsed());
        for key in [
            Key::Up,
            Key::Left,
            Key::Right,
            Key::PageDown,
            Key::NextGroup,
        ] {
            assert_eq!(
                launcher.press(key, &apps, &frecency, NOW, CLOCK, STILL),
                Outcome::Unchanged,
                "{key:?} did something to a bar"
            );
        }
        assert_eq!(
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert!(!launcher.is_collapsed());
        // Escape folds it back before it closes anything.
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert!(launcher.is_open() && launcher.is_collapsed());
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Dismissed
        );
    }

    #[test]
    fn typing_opens_the_bar_and_emptying_the_query_leaves_it_open() {
        let (mut launcher, apps) = typed("fi");
        assert!(!launcher.is_collapsed());
        launcher.press(Key::Clear, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert!(
            !launcher.is_collapsed(),
            "the grid folded away under the user's hands"
        );
    }

    #[test]
    fn pinned_applications_are_on_the_foot_and_not_suggested_again() {
        let apps = apps();
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.set_pinned(vec![
            apps[2].path.clone(),
            PathBuf::from("/apps/uninstalled.desktop"),
        ]);
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert_eq!(launcher.pins_shown(), &[2], "an uninstalled pin was shown");
        assert!(!launcher.suggested().contains(&2));
        assert_eq!(
            launcher.visible()[launcher.suggested().len()],
            Target::App(2),
            "the pins come straight after the tiles"
        );
    }

    #[test]
    fn the_filter_narrows_a_search_by_kind_and_counts_before_narrowing() {
        let (mut launcher, apps) = with_files("fi");
        let frecency = Frecency::new();
        let (found_apps, found_files) = launcher.found();
        assert!(found_apps >= 2 && found_files == 1);

        launcher.press(Key::NextGroup, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.filter(), Filter::Apps);
        assert!(launcher.file_hits().is_empty());

        launcher.press(Key::NextGroup, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.filter(), Filter::Files);
        assert!(launcher.results().is_empty());
        assert!(matches!(launcher.target(), Some(Target::File(_))));
        assert!(
            !launcher.offers_command(),
            "hiding the applications made the query a command"
        );
        assert_eq!(launcher.found(), (found_apps, found_files));

        // With the query gone, the next search starts from everything.
        launcher.press(Key::Clear, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.filter(), Filter::All);
    }

    #[test]
    fn sorting_by_name_orders_the_results_alphabetically() {
        let (mut launcher, apps) = typed("f");
        let frecency = Frecency::new();
        assert_eq!(
            launcher.press_button(Button::Sort, &apps, &frecency, NOW),
            Outcome::Redraw
        );
        assert_eq!(launcher.sort(), Sort::Name);
        let names: Vec<String> = launcher
            .results()
            .iter()
            .map(|i| apps[*i].name.to_lowercase())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn the_style_round_trips_through_its_name() {
        for style in Style::ALL {
            assert_eq!(Style::from_value(style.value()), Some(style));
        }
        assert_eq!(Style::from_value(" arc "), Some(Style::Arc));
        assert_eq!(Style::from_value("orbit"), None);
        assert_eq!(Style::List.stepped(1), Style::Arc);
        assert_eq!(Style::Arc.stepped(1), Style::List);
    }

    #[test]
    fn changing_the_style_puts_the_highlight_back_on_top() {
        let (mut launcher, apps) = typed("f");
        launcher.press(Key::Right, &apps, &Frecency::new(), NOW, CLOCK, STILL);
        assert!(launcher.set_style(Style::Arc));
        assert!(
            !launcher.set_style(Style::Arc),
            "the same style is not a change"
        );
        launcher.reindex(&apps, &Frecency::new(), NOW);
        assert_eq!(launcher.selected(), 0);
    }

    #[test]
    fn on_the_arc_the_arrows_walk_the_slots_by_where_they_sit() {
        let apps = tools(7);
        let frecency = Frecency::new();
        let mut launcher = on_arc(&apps, "");
        assert!(!launcher.is_collapsed(), "the arc has no bar");
        assert_eq!(launcher.selected(), 0, "the best match is highlighted");
        // From the top, Left is the slot to its left — the second rank —
        // and so on to the left end; Up goes the same way along the curve.
        launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(Some(launcher.selected()), rank_at(2));
        launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(Some(launcher.selected()), rank_at(1));
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(Some(launcher.selected()), rank_at(0));
        assert_eq!(
            launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged,
            "past the left end with no page before it"
        );
        for _ in 0..6 {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(Some(launcher.selected()), rank_at(ARC_SLOTS - 1));
        assert_eq!(
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
    }

    #[test]
    fn past_the_end_of_the_arc_is_the_next_page_and_back() {
        let apps = tools(10);
        let frecency = Frecency::new();
        let mut launcher = on_arc(&apps, "");
        for _ in 0..3 {
            launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        }
        assert_eq!(Some(launcher.selected()), rank_at(ARC_SLOTS - 1));
        launcher.press(Key::Right, &apps, &frecency, NOW, CLOCK, STILL);
        // The next page holds three; its left-most slot has the second rank.
        assert_eq!(launcher.selected(), ARC_SLOTS + 1);
        launcher.press(Key::Left, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            Some(launcher.selected()),
            rank_at(ARC_SLOTS - 1),
            "back onto the first page, at its right end"
        );
        launcher.press(Key::PageDown, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(
            launcher.selected(),
            ARC_SLOTS,
            "a page lands on its top slot"
        );
        assert_eq!(
            launcher.press(Key::PageDown, &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Unchanged
        );
        launcher.press(Key::PageUp, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.selected(), 0);
    }

    #[test]
    fn a_category_narrows_the_arc_and_ctrl_arrows_step_through_them() {
        let mut apps = tools(3);
        apps[0].categories = vec!["Development".into()];
        apps[1].categories = vec!["Network".into(), "WebBrowser".into()];
        let frecency = Frecency::new();
        let mut launcher = on_arc(&apps, "");
        assert_eq!(
            launcher.categories(),
            &[Category::All, Category::Development, Category::Internet],
            "an empty category was offered"
        );
        assert_eq!(launcher.visible().len(), 3);
        launcher.press(Key::NextGroup, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.visible(), &[Target::App(0)]);
        launcher.press(Key::NextGroup, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.visible(), &[Target::App(1)]);
        launcher.press(Key::NextGroup, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.visible().len(), 3, "round again to all of them");
        launcher.press(Key::PrevGroup, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        assert_eq!(launcher.category(), 0, "reopening kept the category");
    }

    #[test]
    fn escape_on_the_arc_closes_at_once() {
        let apps = apps();
        let mut launcher = on_arc(&apps, "");
        assert_eq!(
            launcher.press(Key::Dismiss, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Dismissed
        );
    }

    #[test]
    fn the_arc_shows_its_result_in_the_hub_rather_than_as_a_slot() {
        let launcher = on_arc(&apps(), "2+2");
        assert_eq!(launcher.result(), Some("4"));
        assert!(!launcher.visible().contains(&Target::Result));
        assert_eq!(launcher.visible(), &[Target::Command]);
    }

    #[test]
    fn the_arcs_card_runs_an_action_on_a_click_without_tab() {
        let apps = browser();
        let mut launcher = on_arc(&apps, "");
        launcher.set_layout(Layout {
            size: (100, 100),
            menu_hits: vec![
                (Rect::from_xywh(0, 0, 100, 20), 0),
                (Rect::from_xywh(0, 20, 100, 20), 1),
            ],
            ..Layout::default()
        });
        let point = huginn_core::geometry::Point::new(50, 30);
        match launcher.click(point, &apps, &Frecency::new(), NOW, CLOCK, STILL) {
            Outcome::Launch { argv, .. } => {
                assert_eq!(argv, vec!["/bin/browser", "--new-window"]);
            }
            other => panic!("the card's action did not run: {other:?}"),
        }
    }

    #[test]
    fn a_button_answers_a_click_without_launching_anything() {
        let (mut launcher, apps) = with_files("fi");
        launcher.set_layout(Layout {
            size: (100, 100),
            buttons: vec![
                (Rect::from_xywh(0, 0, 50, 20), Button::Filter(Filter::Apps)),
                (Rect::from_xywh(50, 0, 50, 20), Button::PinnedPanel),
            ],
            ..Layout::default()
        });
        let frecency = Frecency::new();
        let point = |x| huginn_core::geometry::Point::new(x, 10);
        assert_eq!(
            launcher.click(point(10), &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::Redraw
        );
        assert_eq!(launcher.filter(), Filter::Apps);
        assert!(launcher.is_open());
        assert_eq!(
            launcher.click(point(60), &apps, &frecency, NOW, CLOCK, STILL),
            Outcome::OpenPinned
        );
    }

    #[test]
    fn a_pointer_on_the_canvas_but_off_everything_drawn_is_not_on_the_launcher() {
        use huginn_core::geometry::Point;
        let layout = Layout {
            size: (200, 200),
            surfaces: vec![Rect::from_xywh(50, 50, 100, 100)],
            ..Layout::default()
        };
        let panel = Rect::from_xywh(0, 0, 200, 200);
        assert_eq!(layout.canvas_point(panel, Point::new(10, 10)), None);
        assert_eq!(
            layout.canvas_point(panel, Point::new(60, 60)),
            Some(Point::new(60, 60))
        );
    }

    #[test]
    fn ctrl_arrows_and_page_keys_are_recognised() {
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Left, true, None),
            Key::PrevGroup
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Right, true, None),
            Key::NextGroup
        );
        assert_eq!(Key::from_keysym(keysyms::KEY_Left, false, None), Key::Left);
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Page_Down, false, None),
            Key::PageDown
        );
        assert_eq!(
            Key::from_keysym(keysyms::KEY_Page_Up, false, None),
            Key::PageUp
        );
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
            ..Layout::default()
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
    fn hovering_a_scrolled_grid_does_not_scroll_it() {
        // Down to the last row: the grid is scrolled to the bottom. The
        // pointer landing on the top drawn tile must highlight that tile and
        // leave the grid alone, or the tiles slide out from under it.
        let apps = tools(30);
        let frecency = Frecency::new();
        let mut launcher = Launcher::default();
        launcher.open(&apps, &frecency, NOW, None, CLOCK, STILL);
        for c in "tool".chars() {
            launcher.press(Key::Insert(c), &apps, &frecency, NOW, CLOCK, STILL);
        }
        for _ in 0..4 {
            launcher.press(Key::Down, &apps, &frecency, NOW, CLOCK, STILL);
        }
        let first = launcher.first_row();
        assert_eq!(first, 2);
        let top = first * COLUMNS;
        launcher.set_layout(stacked(launcher.visible.len()));
        assert_eq!(launcher.hover(on_row(top)), Outcome::Redraw);
        assert_eq!(launcher.selected(), top);
        assert_eq!(launcher.first_row(), first, "hovering scrolled the grid");
        // The keyboard still pulls the grid: Up off the top row slides it.
        launcher.press(Key::Up, &apps, &frecency, NOW, CLOCK, STILL);
        assert_eq!(launcher.first_row(), first - 1);
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
        match launcher.click(on_row(2), &apps, &Frecency::new(), NOW, CLOCK, STILL) {
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
            launcher.click(below, &apps, &Frecency::new(), NOW, CLOCK, STILL),
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
            launcher.click(on_row(1), &apps, &Frecency::new(), NOW, CLOCK, STILL),
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
        let mut launcher = expanded(&apps, &frecency);
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
        let mut launcher = expanded(&apps, &frecency);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.set_layout(with_menu(1, 3));
        match launcher.click(on_menu(1), &apps, &Frecency::new(), NOW, CLOCK, STILL) {
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
        let mut launcher = expanded(&apps, &frecency);
        launcher.press(Key::Actions, &apps, &frecency, NOW, CLOCK, STILL);
        launcher.set_layout(with_menu(1, 3));
        let beside = huginn_core::geometry::Point::new(25, 10);
        assert_eq!(
            launcher.click(beside, &apps, &Frecency::new(), NOW, CLOCK, STILL),
            Outcome::Redraw
        );
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
    /// Things that answer a click but are not in the navigation order: the
    /// list's tabs, sort and chevron, the arc's categories.
    pub(crate) buttons: Vec<(Rect, Button)>,
    /// Where anything is drawn. The arc's canvas is mostly transparent —
    /// the space around the arc, between it and its sidebar and card — and
    /// a pointer there is over the desktop, not the launcher. Empty means
    /// the whole canvas is panel.
    pub(crate) surfaces: Vec<Rect>,
    /// A rectangle, in canvas pixels, that lies wholly inside the panel's
    /// glass, for a panel that is not a rounded rectangle. `None` blurs the
    /// panel inset by its corners. See [`Launcher::blur_region`].
    pub(crate) blur: Option<Rect>,
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
        // Over the canvas but not over anything drawn on it is the desktop.
        let point = Point::new(x as i32, y as i32);
        self.on_surface(point).then_some(point)
    }

    /// The button under `point`, if any. The menu covers what it is over.
    pub(crate) fn button(&self, point: Point) -> Option<Button> {
        if self.menu_hit(point).is_some() {
            return None;
        }
        self.buttons
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, button)| *button)
    }

    /// Whether `point`, in canvas pixels, is on something drawn.
    fn on_surface(&self, point: Point) -> bool {
        self.surfaces.is_empty() || self.surfaces.iter().any(|rect| rect.contains(point))
    }

    /// `rect`, in canvas pixels, on the output where the panel was placed at
    /// `panel`: the inverse of [`Self::canvas_point`].
    pub(crate) fn to_output(&self, panel: Rect, rect: Rect) -> Rect {
        if self.size.0 <= 0 || self.size.1 <= 0 {
            return Rect::ZERO;
        }
        let (sx, sy) = (
            panel.w() as f32 / self.size.0 as f32,
            panel.h() as f32 / self.size.1 as f32,
        );
        Rect::from_xywh(
            panel.x() + (rect.x() as f32 * sx).ceil() as i32,
            panel.y() + (rect.y() as f32 * sy).ceil() as i32,
            (rect.w() as f32 * sx).floor() as i32,
            (rect.h() as f32 * sy).floor() as i32,
        )
    }
}

/// Panel width at a 1080p output, in pixels.
pub(crate) const WIDTH: f32 = 560.0;
/// Padding inside the panel's border.
pub(crate) const PAD: f32 = 18.0;
/// Text size at a 1080p output.
pub(crate) const BASE_SIZE: f32 = 16.0;
/// Rows of the list's result grid drawn at once. Beyond this the answer was
/// not near the top, and another keystroke is faster than another screenful.
pub(crate) const GRID_ROWS: usize = 3;
/// How many suggestions the list shows before anything is typed: one row.
const SUGGESTED: usize = 6;
/// Tiles per row of the list's grid.
pub(crate) const COLUMNS: usize = 6;
/// How many recently launched applications the list's foot shows.
const RECENT: usize = 3;
/// How many pinned applications the list's foot shows; the rest are a click
/// away on the pin bar.
const PINS_SHOWN: usize = 6;
/// How many matching files a search lists after the applications. None are
/// listed until the last query term is [`raven_desktop::files::MIN_TERM`]
/// characters long; see [`Launcher::refresh`].
const FILES: usize = 12;
/// Slots on the arc; a category or a search with more than this is paged.
pub(crate) const ARC_SLOTS: usize = 7;
/// Which slot of the arc, counted from its left end, each rank sits in: the
/// best match at the top, then outward — left, right, left, right — so the
/// eye starts where the answer most likely is and moves out from there.
pub(crate) const RANK_POS: [usize; ARC_SLOTS] = [3, 2, 4, 1, 5, 0, 6];

/// The rank that sits in the arc's slot `position`, counted from its left
/// end, if any.
pub(crate) fn rank_at(position: usize) -> Option<usize> {
    RANK_POS.iter().position(|p| *p == position)
}
/// The theme icon drawn beside a file.
const FILE_ICON: &str = "text-x-generic";
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
pub(crate) const ALPHA: u8 = crate::theme::PANEL_ALPHA;
// Legibility bounds the alpha from below; above the upper bound the panel
// hides the blur behind it and the blur pass is pure cost.
const _: () = assert!(ALPHA >= 0xC0 && ALPHA <= 0xE0);
/// The footer's key hints, in the order they are read. The grid also
/// answers to sideways arrows, and says so; the menu says what it does.
const GRID_HINTS: &[(&str, &str)] = &[
    ("←↑↓→", "Move"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Esc", "Collapse"),
];
const LIST_HINTS: &[(&str, &str)] = &[
    ("←↑↓→", "Move"),
    ("Enter", "Open"),
    ("Tab", "Actions"),
    ("Ctrl ←→", "Filter"),
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
/// The actions menu's last item, which puts the entry on the pin bar
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
    style: Style,
) -> Rect {
    /// How small the panel gets at the start of the motion.
    const MIN_SCALE: f32 = 0.86;
    /// Where the list's top edge hangs, as a fraction of the output's height.
    const LIST_TOP: f32 = 0.15;

    let (w, h) = panel;
    // The list hangs from a fixed height rather than being centred: it
    // opens from a bar into a grid, and a centred panel would jump upwards
    // by half of whatever it grew. The arc is a fixed size and sits centred.
    let y = match style {
        Style::List => (output.y() + (output.h() as f32 * LIST_TOP) as i32)
            .min(output.bottom() - h)
            .max(output.y()),
        Style::Arc => output.y() + (output.h() - h).max(0) / 2,
    };
    let full = Rect::from_xywh(output.x() + (output.w() - w).max(0) / 2, y, w, h);
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
    match launcher.style() {
        Style::List => list::compose(launcher, apps, text, icons, pixmaps, output, density),
        Style::Arc => arc::compose(launcher, apps, text, icons, pixmaps, output, density),
    }
}

/// The measurements every panel that looks like the launcher is laid out
/// with, in canvas pixels.
///
/// One struct rather than a dozen `let`s at the top of each `compose`, so a
/// panel drawn beside the launcher is drawn to exactly the launcher's
/// proportions rather than to a copy of them that drifts by a constant. The
/// pin bar (see [`crate::pinned`]) has a shape of its own but takes its type
/// size, row height and scale from here, which is why its menu reads as the
/// launcher's menu does.
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
    /// Height of a list row.
    pub(crate) row: f32,
    /// The width inside the padding. The panel's own width is [`WIDTH`] at
    /// this scale, or whatever [`Self::with_width`] was given; only the room
    /// it leaves is measured from, so only that is kept.
    pub(crate) inner: f32,
    /// Space between tiles, and between sections.
    pub(crate) gap: f32,
    /// Height of a section heading.
    pub(crate) heading: f32,
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
            row: size * 2.2,
            inner: width as f32 - pad * 2.0,
            gap: TILE_GAP * scale,
            heading: size * 1.9,
        }
    }

    /// The same measurements for a panel `width` canvas pixels wide.
    pub(crate) fn with_width(self, width: usize) -> Self {
        Self {
            inner: width as f32 - self.pad * 2.0,
            ..self
        }
    }

    /// A menu of `items` actions: its heading, the rows, a little air below.
    pub(crate) fn menu_height(&self, items: usize) -> f32 {
        self.heading + self.row * items as f32 + self.gap
    }
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
    // A menu floats over the panel, so it is the material one layer up:
    // near-opaque, with a brighter hairline so its edge reads against the
    // panel it sits on.
    canvas.fill_rounded(
        mx as usize,
        my as usize,
        menu_w as usize,
        menu_h as usize,
        corner,
        crate::theme::BACKGROUND.with_alpha(0xF6),
    );
    canvas.stroke_rounded(
        mx as usize,
        my as usize,
        menu_w as usize,
        menu_h as usize,
        corner,
        edge,
        crate::theme::HAIRLINE.with_alpha(0x44),
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
                crate::theme::selection(),
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
            crate::theme::selection(),
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
        let rect = placement(OUTPUT, PANEL, Some(ICON), 1.0, Style::Arc);
        assert_eq!((rect.w(), rect.h()), PANEL, "it did not reach full size");
        let left = rect.x();
        let right = OUTPUT.w() - (rect.x() + rect.w());
        assert!((left - right).abs() <= 1, "off centre by {}", left - right);
    }

    #[test]
    fn it_starts_at_the_dock_icon_and_travels_to_the_centre() {
        // §4: "fade + scale up from the dock icon's position".
        let start = placement(OUTPUT, PANEL, Some(ICON), 0.0, Style::Arc);
        let icon_centre = (ICON.x() + ICON.w() / 2, ICON.y() + ICON.h() / 2);
        let start_centre = (start.x() + start.w() / 2, start.y() + start.h() / 2);
        assert_eq!(start_centre, icon_centre, "it did not start at the icon");

        // And moves monotonically toward the middle of the screen.
        let mut previous = start_centre.1;
        for step in 1..=10 {
            let rect = placement(OUTPUT, PANEL, Some(ICON), step as f32 / 10.0, Style::Arc);
            let y = rect.y() + rect.h() / 2;
            assert!(y <= previous, "it moved back down at {step}");
            previous = y;
        }
    }

    #[test]
    fn it_scales_up_rather_than_appearing_at_full_size() {
        let small = placement(OUTPUT, PANEL, Some(ICON), 0.0, Style::Arc);
        let big = placement(OUTPUT, PANEL, Some(ICON), 1.0, Style::Arc);
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
        let start = placement(OUTPUT, PANEL, Some(ICON), 0.0, Style::Arc);
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
        let start = placement(OUTPUT, PANEL, None, 0.0, Style::Arc);
        let full = placement(OUTPUT, PANEL, None, 1.0, Style::Arc);
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
            let opening = placement(OUTPUT, PANEL, Some(ICON), t, Style::Arc);
            let closing = placement(OUTPUT, PANEL, Some(ICON), t, Style::Arc);
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
        // A solid accent tile is unreadable and was what the bug produced.
        let (canvas, _) = drawn("", 1);
        let accent = crate::theme::accent().to_rgba_bytes();
        let flooded = canvas
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] == accent[0] && p[1] == accent[1] && p[2] == accent[2])
            .count();
        // The caret and the field's edge are near-solid accent; a flooded
        // tile would be thousands of pixels.
        assert!(
            flooded < 4_000,
            "{flooded} solid-accent pixels; the tile is flooded"
        );
    }

    #[test]
    fn the_panel_grows_and_shrinks_with_what_it_shows() {
        let (bar, _) = drawn("", 0);
        let (grid, _) = drawn("", 1);
        assert!(
            bar.height < grid.height,
            "the bar did not open into the grid"
        );
        let two_rows = composed(&tools(12), "tool");
        let one_row = composed(&apps(), "raven");
        assert!(
            one_row.height < two_rows.height,
            "the grid did not grow with its results"
        );
    }

    #[test]
    fn the_list_hangs_from_the_same_place_as_it_opens() {
        // Centred, it would jump up by half of whatever it grew.
        let (bar, _) = drawn("", 0);
        let (grid, _) = drawn("", 1);
        let top =
            |height: usize| placement(OUTPUT, (760, height as i32), None, 1.0, Style::List).y();
        assert_eq!(
            top(bar.height),
            top(grid.height),
            "the list moved as it opened"
        );
        assert!(
            top(grid.height) < OUTPUT.h() / 4,
            "the list is not near the top"
        );
    }

    /// `count` applications answering to "tool".
    fn tools(count: usize) -> Vec<Entry> {
        (1..=count)
            .map(|n| entry(&format!("Tool {n:02}"), &format!("/bin/tool{n}")))
            .collect()
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
        // Thirty matches, three rows of six drawn: the panel is as tall as
        // with eighteen, and stays that tall however far the grid scrolls.
        let many = tools(30);
        let screenful = many[..COLUMNS * GRID_ROWS].to_vec();
        let capped = composed(&many, "tool");
        let full = composed(&screenful, "tool");
        assert_eq!(
            capped.height, full.height,
            "the panel grew past GRID_ROWS rows"
        );

        let mut text = Text::new();
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        launcher.open(&many, &frecency, NOW, None, CLOCK, STILL);
        for c in "tool".chars() {
            launcher.press(Key::Insert(c), &many, &frecency, NOW, CLOCK, STILL);
        }
        for _ in 0..4 {
            launcher.press(Key::Down, &many, &frecency, NOW, CLOCK, STILL);
        }
        assert!(launcher.first_row() > 0, "the grid did not scroll");
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let scrolled = compose(
            &launcher,
            &many,
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
        // Open the bar into the grid, which typing would do anyway.
        launcher.press(Key::Down, apps, &frecency, NOW, CLOCK, STILL);
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
        assert!(second.0.x() > first.0.x(), "the foot is out of order");
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
        assert_eq!(
            layout.hits.len(),
            launcher.results().len() + launcher.file_hits().len()
        );
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
            launcher.results().len(),
            1,
            "the fixture did not narrow to one tile"
        );
        assert_eq!(layout.menu_hits.len(), 3, "Open, the action, and Pin");
        let (_, bare) = laid_out(&apps, &query, &[], false);
        assert!(
            bare.size.1 <= layout.size.1,
            "the panel shrank for the menu"
        );
        // Below the field, and inside the canvas.
        let field_bottom = (9.0 + 50.0) as i32;
        for (rect, item) in &layout.menu_hits {
            assert!(
                rect.y() >= field_bottom && rect.bottom() <= layout.size.1,
                "menu item {item} at {rect:?} is off the body"
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

    // -- The arc ------------------------------------------------------------

    fn arc_laid_out(apps: &[Entry], query: &str) -> (Launcher, Canvas, Layout) {
        let mut text = Text::new();
        let frecency = Frecency::default();
        let mut launcher = Launcher::default();
        launcher.set_style(Style::Arc);
        launcher.open(apps, &frecency, NOW, None, CLOCK, STILL);
        for c in query.chars() {
            launcher.press(Key::Insert(c), apps, &frecency, NOW, CLOCK, STILL);
        }
        let icons = Icons::discover(crate::theme::ICON_THEME);
        let mut pixmaps = Pixmaps::new();
        let (canvas, layout) = compose(
            &launcher,
            apps,
            &mut text,
            &icons,
            &mut pixmaps,
            Rect::from_xywh(0, 0, 1920, 1080),
            1,
        );
        (launcher, canvas, layout)
    }

    #[test]
    fn every_slot_on_the_arc_is_a_hit_and_the_best_match_is_on_top() {
        let apps = apps();
        let (_, _, layout) = arc_laid_out(&apps, "");
        assert_eq!(layout.hits.len(), apps.len());
        let top = layout.hits.iter().min_by_key(|(rect, _)| rect.y()).unwrap();
        assert_eq!(top.1, 0, "the top slot is not the first rank");
        for (rect, index) in &layout.hits {
            assert_eq!(layout.hit(centre(*rect)), Some(*index));
        }
    }

    #[test]
    fn off_the_glass_the_arc_is_transparent_and_not_the_launcher() {
        let (_, canvas, layout) = arc_laid_out(&apps(), "");
        assert_eq!(canvas.pixels[3], 0, "the canvas's corner is painted");
        let panel = Rect::from_xywh(0, 0, layout.size.0, layout.size.1);
        assert_eq!(layout.canvas_point(panel, Point::new(2, 2)), None);
        let hub = layout.blur.expect("the arc names its blur").center();
        assert!(
            layout.canvas_point(panel, hub).is_some(),
            "the hub is not the launcher"
        );
    }

    #[test]
    fn the_arcs_blur_lies_on_its_glass() {
        let (_, canvas, layout) = arc_laid_out(&apps(), "");
        let blur = layout.blur.expect("the arc names its blur");
        for (x, y) in [
            (blur.x(), blur.y()),
            (blur.right() - 1, blur.y()),
            (blur.x(), blur.bottom() - 1),
            (blur.right() - 1, blur.bottom() - 1),
        ] {
            let alpha = canvas.pixels[(y as usize * canvas.stride + x as usize) * 4 + 3];
            assert!(
                alpha > 0x80,
                "the blur's corner ({x}, {y}) is off the glass: {alpha}"
            );
        }
    }

    #[test]
    fn the_arcs_card_offers_open_then_the_rest_of_the_menu() {
        let mut apps = apps();
        apps[0].actions.push(raven_desktop::entry::Action {
            id: "private".into(),
            name: "Private Window".into(),
            exec: "firefox --private-window".into(),
            icon: None,
        });
        let (launcher, _, layout) = arc_laid_out(&apps, "fire");
        let entry = &apps[launcher.selection().expect("Firefox is highlighted")];
        assert_eq!(layout.menu_hits.len(), launcher.menu_items(entry).len());
        assert_eq!(layout.menu_hits[0].1, 0, "Open is not the button");
    }

    #[test]
    fn the_arcs_sidebar_is_categories_until_something_is_typed() {
        let (_, _, layout) = arc_laid_out(&apps(), "");
        assert!(
            layout
                .buttons
                .iter()
                .any(|(_, b)| *b == Button::Category(0))
        );
        let (_, _, searching) = arc_laid_out(&apps(), "f");
        assert!(
            searching
                .buttons
                .iter()
                .any(|(_, b)| *b == Button::Filter(Filter::Files))
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
        // `LAUNCHER_STYLE=arc`: the arc rather than the list.
        if std::env::var("LAUNCHER_STYLE").is_ok_and(|s| s.eq_ignore_ascii_case("arc")) {
            launcher.set_style(Style::Arc);
        }
        // `LAUNCHER_PINS=Firefox,Files`: pin those, for the list's foot.
        launcher.set_pinned(
            std::env::var("LAUNCHER_PINS")
                .unwrap_or_default()
                .split(',')
                .filter_map(|name| apps.iter().find(|a| a.name.eq_ignore_ascii_case(name)))
                .map(|app| app.path.clone())
                .collect(),
        );
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
        if std::env::var_os("LAUNCHER_ALL_APPS").is_some() {
            launcher.press_button(Button::AllApps, &apps, &frecency, now);
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
            let inverse = 255 - u32::from(pixel[3]);
            for (channel, backdrop) in [48_u32, 44, 86].into_iter().enumerate() {
                let value = u32::from(pixel[channel]) + backdrop * inverse / 255;
                ppm.push(value.min(255) as u8);
            }
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
            let panel_w = (WIDTH * m.scale) as usize;
            let mut canvas = Canvas::new(panel_w, 600);
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
                panel_w,
                600,
                RADIUS * m.scale,
                crate::theme::BACKGROUND.with_alpha(ALPHA),
            );
            println!("fill_rounded {panel_w}x600: {:?}", t.elapsed());
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
        let half = placement(output, (700, 500), origin, 0.5, Style::Arc);
        let full = placement(output, (700, 500), origin, 1.0, Style::Arc);
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
