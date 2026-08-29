//! Ranking: which application the user meant.
//!
//! The launcher is search-first — type two characters, press Enter, done — so
//! the entire experience is whether the right thing is top of the list after
//! two keystrokes. That makes the ranking the feature, not a detail of it.
//!
//! Two signals, in strict priority:
//!
//! 1. **How well the query matches**, as a [`Quality`] tier. An exact name wins
//!    over a prefix, a prefix over a word-start, a word-start over a scattered
//!    subsequence. Tiers are compared before anything else, so no amount of
//!    past use promotes a vague match over a precise one — typing `fi` must not
//!    open Firefox just because Firefox is opened often.
//! 2. **Frecency** breaks ties within a tier, so among the things that match
//!    equally well the one actually used comes first.
//!
//! Everything here is pure: the clock is a parameter and the entries are a
//! slice. That is deliberate — ranking is the part most worth testing and the
//! part that would otherwise need a running desktop to try.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::entry::Entry;

/// How well a query matched, best first.
///
/// Ordered so that `Exact > Prefix > WordPrefix > Subsequence`, which is what
/// `derive(Ord)` gives for variants declared in that order once reversed — see
/// [`Quality::rank`], which is what actually does the comparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    /// The query is a scattered subsequence: `frfx` in "Firefox".
    Subsequence,
    /// The query starts a word other than the first: `term` in "Raven Terminal".
    WordPrefix,
    /// The query starts the name: `fire` in "Firefox".
    Prefix,
    /// The query is the whole name.
    Exact,
}

/// Where a match was found. A hit on the name beats a hit on a keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    /// `Keywords`.
    Secondary,
    /// `GenericName` — what the application is.
    Generic,
    /// `Name` — what it is called.
    Name,
}

/// The two signals combined into one comparable number.
///
/// Quality and field cannot be compared one after the other, in either order,
/// and this is not obvious until it is wrong in both directions:
///
/// - **Quality first** ranks a keyword above a name whenever the keyword happens
///   to match more tightly. Searching `te` found *Vim* before *Raven Terminal*
///   on a real desktop, because Vim's keyword `text` starts with `te` while the
///   name "Raven Terminal" only contains `te` at a word boundary.
/// - **Field first** ranks any name above any keyword, so a query that is a
///   scattered subsequence of some unrelated name beats an exact keyword hit.
///   `chat` would find "Cheat Sheet" before the chat client.
///
/// So they are weighted instead: a hit on a lesser field is worth roughly one
/// tier less than the same hit on the name, which is enough to keep a name's
/// word-start ahead of a keyword's prefix without letting a name's vague
/// subsequence beat a keyword's exact hit.
fn rank(quality: Quality, field: Field) -> u32 {
    let base = match quality {
        Quality::Exact => 100,
        Quality::Prefix => 80,
        Quality::WordPrefix => 60,
        Quality::Subsequence => 30,
    };
    let penalty = match field {
        Field::Name => 0,
        Field::Generic => 30,
        Field::Secondary => 35,
    };
    base - penalty.min(base)
}

/// One ranked result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Index into the slice that was searched.
    pub index: usize,
    /// How well it matched.
    pub quality: Quality,
    /// Its frecency at the time of the search.
    pub frecency: f64,
}

/// How often and how recently each application has been launched.
///
/// Stored as one number per application rather than a list of timestamps: a
/// running score decayed on read is the same shape of answer as summing
/// per-launch weights, and it costs two fields instead of an unbounded history.
#[derive(Debug, Clone, Default)]
pub struct Frecency {
    records: HashMap<PathBuf, Record>,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    /// The score as of `updated`, before any decay since.
    score: f64,
    /// Unix seconds when `score` was last written.
    updated: u64,
}

/// Time for a launch to lose half its weight.
///
/// Two weeks: long enough that a tool used every few days stays near the top
/// through a quiet week, short enough that last month's habit does not outrank
/// this week's. The exact figure matters less than that it is finite — a score
/// that never decays makes the launcher a museum of what you used to run.
const HALF_LIFE_SECS: f64 = 14.0 * 24.0 * 60.0 * 60.0;

impl Frecency {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was launched at `now` (unix seconds).
    ///
    /// The existing score is decayed to the present before the new launch is
    /// added, so one launch is always worth exactly 1.0 whenever it happens.
    /// Adding first and decaying later would make an old score worth more
    /// simply for having been added to more often.
    pub fn record(&mut self, path: &Path, now: u64) {
        let score = self.score(path, now) + 1.0;
        self.records.insert(
            path.to_owned(),
            Record {
                score,
                updated: now,
            },
        );
    }

    /// The decayed score for `path` at `now`. Zero if never launched.
    pub fn score(&self, path: &Path, now: u64) -> f64 {
        self.records
            .get(path)
            .map_or(0.0, |record| record.decayed(now))
    }

    /// When `path` was last launched, in unix seconds. `None` if never.
    ///
    /// Frecency answers "what do you use"; this answers "what did you just
    /// use", which is a different list — a tool run once a minute ago sits
    /// nowhere near the top by score but is the likeliest thing to want back.
    pub fn last_used(&self, path: &Path) -> Option<u64> {
        self.records.get(path).map(|r| r.updated)
    }

    /// How many applications have ever been launched.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Forget every application whose score at `now` has decayed below
    /// [`PRUNE_BELOW`].
    ///
    /// Decay never reaches zero, so without this the file on disk would keep
    /// every application ever launched. A score that small is also below
    /// anything the ranking can tell apart from "never used", so nothing the
    /// user can see changes — except that a one-off launch from last year no
    /// longer counts as "recent" for the launcher's Recent rows either, which
    /// is what anyone would expect of it.
    pub fn prune(&mut self, now: u64) {
        self.records
            .retain(|_, record| record.decayed(now) >= PRUNE_BELOW);
    }

    /// The on-disk form: one `updated\tscore\tpath` line per application.
    ///
    /// Text rather than a serialisation crate because there is nothing to
    /// serialise but three columns, and a file the user can read and repair
    /// with a text editor is worth more than a schema. Tab-separated because
    /// a `.desktop` path can contain spaces but not, in practice, a tab; the
    /// path is last so that it can be anything at all. Lines are sorted so
    /// that two saves of the same state produce the same bytes.
    pub fn to_text(&self) -> String {
        let mut lines: Vec<String> = self
            .records
            .iter()
            .map(|(path, record)| {
                format!(
                    "{}\t{}\t{}\n",
                    record.updated,
                    record.score,
                    path.to_string_lossy()
                )
            })
            .collect();
        lines.sort();
        lines.concat()
    }

    /// Read back what [`Frecency::to_text`] wrote.
    ///
    /// Forgiving on purpose: a blank line, a comment, a line with the wrong
    /// number of columns, a score that is not a number — each is skipped, not
    /// fatal. The file is a cache of habits, and losing one line of it is
    /// nothing compared to losing all of it because one line was odd. A
    /// negative or non-finite score is dropped too, since [`Frecency::record`]
    /// could never have written one and it would otherwise poison the ranking.
    pub fn parse(text: &str) -> Self {
        let mut records = HashMap::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.splitn(3, '\t');
            let (Some(updated), Some(score), Some(path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(updated), Ok(score)) =
                (updated.trim().parse::<u64>(), score.trim().parse::<f64>())
            else {
                continue;
            };
            if !score.is_finite() || score < 0.0 || path.is_empty() {
                continue;
            }
            records.insert(PathBuf::from(path), Record { score, updated });
        }
        Self { records }
    }

    /// Load from `path`, treating a missing file as an empty history.
    ///
    /// A file that cannot be read for any other reason is an error, so that
    /// the caller can log it; but the caller should still start with an empty
    /// history rather than refuse to run, and this is shaped to make that the
    /// obvious thing to write.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }

    /// Write to `path`, creating its directory, without ever leaving a
    /// half-written file behind.
    ///
    /// Written to a sibling temporary file and renamed over the target, since
    /// a rename is atomic on every filesystem this runs on and a truncate-then-
    /// write is not: a crash or a power cut between the two would leave an
    /// empty file, and an empty file is every habit forgotten. The temporary
    /// file carries the process id so that two compositors sharing a home
    /// directory cannot trample each other's half-written file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "frecency".to_owned());
        let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let result =
            std::fs::write(&temp, self.to_text()).and_then(|()| std::fs::rename(&temp, path));
        if result.is_err() {
            // Best effort: the error being reported is the interesting one.
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

impl Record {
    /// The score decayed from `updated` to `now`.
    fn decayed(self, now: u64) -> f64 {
        // A clock that went backwards must not amplify a score. Saturating to
        // zero elapsed means the score is simply not decayed.
        let elapsed = now.saturating_sub(self.updated) as f64;
        self.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS)
    }
}

/// Below this decayed score a record is dropped on [`Frecency::prune`].
///
/// One launch takes a little over six half-lives — three months — to fall
/// this far, which is long past the point where it could move anything in
/// the ranking.
const PRUNE_BELOW: f64 = 0.01;

/// Rank `entries` against `query`, best first.
///
/// An empty query is not "no results" but "no opinion": everything is returned
/// ordered by frecency alone, which is what fills the launcher's grid of recent
/// applications before a single key is pressed.
pub fn search(entries: &[Entry], query: &str, frecency: &Frecency, now: u64) -> Vec<Hit> {
    let query = query.trim().to_lowercase();

    let mut hits: Vec<(Hit, u32, usize, String)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let (quality, field) = if query.is_empty() {
                (Quality::Subsequence, Field::Name)
            } else {
                best_match(entry, &query)?
            };
            Some((
                Hit {
                    index,
                    quality,
                    frecency: frecency.score(&entry.path, now),
                },
                rank(quality, field),
                entry.name.chars().count(),
                entry.name.to_lowercase(),
            ))
        })
        .collect();

    hits.sort_by(|a, b| {
        // Match strength first and frecency second, never the other way round:
        // use must break ties between comparable matches, not overrule a
        // better one.
        b.1.cmp(&a.1)
            .then(
                b.0.frecency
                    .partial_cmp(&a.0.frecency)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            // Shorter names first: for `fi`, "Files" is likelier than "File
            // Roller Archive Manager". Skipped for an empty query, where every
            // entry matches equally and ordering by length is meaningless —
            // the grid would read as a random selection rather than a list.
            .then(if query.is_empty() {
                std::cmp::Ordering::Equal
            } else {
                a.2.cmp(&b.2)
            })
            // Alphabetical last, so an unused desktop still ranks the same way
            // twice rather than in HashMap order.
            .then(a.3.cmp(&b.3))
    });

    hits.into_iter().map(|(hit, ..)| hit).collect()
}

/// The best (quality, field) this entry offers for `query`, if any.
///
/// "Best" by [`rank`], not by comparing the pair as a tuple. A tuple compares
/// quality first, which makes an entry pick its own keyword over its own name
/// whenever the keyword matches more tightly — "Raven Terminal" chose its
/// `terminal` keyword (a prefix, but a keyword) over its own name (a word
/// start, but the name), and so scored 45 where it should have scored 60.
/// Selecting here by a different rule than the one that sorts is how a fix to
/// the sort can leave the bug exactly where it was.
fn best_match(entry: &Entry, query: &str) -> Option<(Quality, Field)> {
    let mut best: Option<(Quality, Field)> = None;
    let mut offer = |quality: Option<Quality>, field: Field| {
        if let Some(quality) = quality
            && best.is_none_or(|(q, f)| rank(quality, field) > rank(q, f))
        {
            best = Some((quality, field));
        }
    };

    offer(match_text(&entry.name, query), Field::Name);
    if let Some(generic) = &entry.generic_name {
        offer(match_text(generic, query), Field::Generic);
    }
    for keyword in &entry.keywords {
        offer(match_text(keyword, query), Field::Secondary);
    }
    best
}

/// How well `query` matches `text`, if at all.
fn match_text(text: &str, query: &str) -> Option<Quality> {
    match_lowercase(&text.to_lowercase(), query)
}

/// [`match_text`] for a `text` already lowercased — for an index that
/// lowercases each name once rather than on every keystroke.
pub(crate) fn match_lowercase(text: &str, query: &str) -> Option<Quality> {
    if text == query {
        return Some(Quality::Exact);
    }
    if text.starts_with(query) {
        return Some(Quality::Prefix);
    }
    // A word start anywhere in the name. "term" should find "Raven Terminal",
    // which is the single most common way people search for an application
    // whose vendor prefixed its name.
    if text
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| !word.is_empty() && word.starts_with(query))
    {
        return Some(Quality::WordPrefix);
    }
    is_subsequence(text, query).then_some(Quality::Subsequence)
}

/// Whether every character of `query` appears in `text`, in order.
fn is_subsequence(text: &str, query: &str) -> bool {
    let mut chars = text.chars();
    query.chars().all(|wanted| chars.any(|c| c == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;
    const DAY: u64 = 24 * 60 * 60;

    fn entry(name: &str, generic: Option<&str>, keywords: &[&str]) -> Entry {
        Entry {
            name: name.to_owned(),
            comment: None,
            generic_name: generic.map(str::to_owned),
            icon: None,
            exec: "/bin/true".to_owned(),
            categories: Vec::new(),
            keywords: keywords.iter().map(|k| (*k).to_owned()).collect(),
            terminal: false,
            startup_wm_class: None,
            path: PathBuf::from(format!("/apps/{name}.desktop")),
            actions: Vec::new(),
        }
    }

    /// A plausible little desktop to rank against.
    fn apps() -> Vec<Entry> {
        vec![
            entry("Firefox", Some("Web Browser"), &["internet", "www"]),
            entry("Files", Some("File Manager"), &["folder"]),
            entry("File Roller", Some("Archive Manager"), &["zip"]),
            entry(
                "Raven Terminal",
                Some("Terminal Emulator"),
                &["shell", "console"],
            ),
            entry("Fractal", Some("Matrix Client"), &["chat"]),
        ]
    }

    fn names(hits: &[Hit], apps: &[Entry]) -> Vec<String> {
        hits.iter().map(|h| apps[h.index].name.clone()).collect()
    }

    #[test]
    fn an_exact_name_wins_outright() {
        let apps = apps();
        let hits = search(&apps, "files", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Files");
    }

    #[test]
    fn a_two_character_query_puts_something_sensible_first() {
        // The acceptance criterion is a two-character query reaching the right
        // app; "fi" must not surface Fractal ahead of Files or Firefox.
        let apps = apps();
        let hits = search(&apps, "fi", &Frecency::new(), NOW);
        let top = &names(&hits, &apps)[..2];
        assert!(
            top.contains(&"Files".to_owned()) && top.contains(&"Firefox".to_owned()),
            "two-character query ranked badly: {:?}",
            names(&hits, &apps)
        );
    }

    #[test]
    fn a_prefix_beats_a_word_start_which_beats_a_scattered_match() {
        let apps = apps();
        let hits = search(&apps, "fr", &Frecency::new(), NOW);
        let ranked = names(&hits, &apps);
        let at = |n: &str| ranked.iter().position(|r| r == n).expect("present");
        // "Fractal" is a prefix; "File Roller" only matches as a subsequence.
        assert!(at("Fractal") < at("File Roller"), "{ranked:?}");
    }

    #[test]
    fn a_vendor_prefixed_name_is_findable_by_its_real_word() {
        // The commonest way people search: "term", not "raven".
        let apps = apps();
        let hits = search(&apps, "term", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Raven Terminal");
    }

    #[test]
    fn what_an_application_is_can_be_searched_as_well_as_what_it_is_called() {
        // Someone who cannot remember "Fractal" still knows they want chat.
        let apps = apps();
        let hits = search(&apps, "matrix", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Fractal");

        let hits = search(&apps, "chat", &Frecency::new(), NOW);
        assert_eq!(
            names(&hits, &apps)[0],
            "Fractal",
            "keywords are searched too"
        );
    }

    #[test]
    fn a_names_word_start_beats_another_apps_keyword_prefix() {
        // Regression, found against a real desktop: searching "te" returned
        // Vim and a V4L2 test utility ahead of Raven Terminal, because a
        // keyword matching as a prefix outscored a name matching at a word
        // boundary. Ranking quality strictly before field does that.
        let apps = vec![
            entry("Vim", Some("Text Editor"), &["Text", "editor"]),
            entry("Qt V4L2 test Utility", None, &["video"]),
            entry("Raven Terminal", None, &["terminal", "console"]),
        ];
        let hits = search(&apps, "te", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Raven Terminal");
    }

    #[test]
    fn an_entry_picks_its_own_best_field_by_the_same_rule_that_sorts() {
        // "Raven Terminal" can match on its name (word start) or its own
        // `terminal` keyword (prefix). Choosing between them by a different
        // rule than the sort uses means the entry hands in a weaker score than
        // it has, and no amount of fixing the sort recovers it.
        let apps = vec![entry("Raven Terminal", None, &["terminal"])];
        let hits = search(&apps, "te", &Frecency::new(), NOW);
        assert_eq!(
            hits[0].quality,
            Quality::WordPrefix,
            "picked the keyword over the name"
        );
    }

    #[test]
    fn a_keywords_exact_hit_still_beats_an_unrelated_names_subsequence() {
        // The other direction, and why field cannot simply outrank quality:
        // "chat" is scattered through "Cheat Sheet" and is exactly a keyword
        // of the chat client.
        let apps = vec![
            entry("Cheat Sheet", None, &[]),
            entry("Fractal", None, &["chat"]),
        ];
        let hits = search(&apps, "chat", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Fractal");
    }

    #[test]
    fn a_name_hit_outranks_a_keyword_hit() {
        let apps = vec![entry("Zebra", None, &["files"]), entry("Files", None, &[])];
        let hits = search(&apps, "files", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Files");
    }

    #[test]
    fn frecency_never_overrules_a_better_match() {
        // The rule the whole ordering hangs on. Firefox launched constantly
        // must still not win the query "files".
        let apps = apps();
        let mut frecency = Frecency::new();
        for _ in 0..500 {
            frecency.record(&apps[0].path, NOW);
        }
        let hits = search(&apps, "files", &frecency, NOW);
        assert_eq!(
            names(&hits, &apps)[0],
            "Files",
            "heavy use promoted a worse match"
        );
    }

    #[test]
    fn frecency_breaks_ties_between_equally_good_matches() {
        // Both are a prefix match on "al", so quality cannot separate them and
        // use is the only thing left to go on.
        let apps = vec![entry("Alpha", None, &[]), entry("Alpine", None, &[])];
        let mut frecency = Frecency::new();
        frecency.record(&apps[1].path, NOW);
        let hits = search(&apps, "al", &frecency, NOW);
        assert_eq!(names(&hits, &apps)[0], "Alpine");

        // And with no use at all, the shorter name leads — deterministically.
        let hits = search(&apps, "al", &Frecency::new(), NOW);
        assert_eq!(names(&hits, &apps)[0], "Alpha");
    }

    #[test]
    fn an_empty_query_ranks_by_use_alone() {
        // This is the grid of recent applications before anything is typed.
        let apps = apps();
        let mut frecency = Frecency::new();
        frecency.record(&apps[3].path, NOW);
        frecency.record(&apps[3].path, NOW);
        frecency.record(&apps[1].path, NOW);

        let hits = search(&apps, "", &frecency, NOW);
        assert_eq!(hits.len(), apps.len(), "an empty query hides nothing");
        assert_eq!(names(&hits, &apps)[0], "Raven Terminal");
        assert_eq!(names(&hits, &apps)[1], "Files");
    }

    #[test]
    fn a_query_that_matches_nothing_returns_nothing() {
        let apps = apps();
        assert!(search(&apps, "qqqzzz", &Frecency::new(), NOW).is_empty());
    }

    #[test]
    fn ranking_is_case_insensitive() {
        let apps = apps();
        let lower = search(&apps, "firefox", &Frecency::new(), NOW);
        let upper = search(&apps, "FireFox", &Frecency::new(), NOW);
        assert_eq!(names(&lower, &apps), names(&upper, &apps));
    }

    #[test]
    fn one_launch_is_worth_the_same_whenever_it_happens() {
        // The decay is applied before the increment, so a score cannot be
        // inflated simply by having been added to over a longer period.
        let path = Path::new("/apps/x.desktop");
        let mut a = Frecency::new();
        a.record(path, NOW);

        let mut b = Frecency::new();
        b.record(path, NOW - 400 * DAY);
        b.record(path, NOW);

        // b has one ancient launch plus one now; a has one now. b should be
        // barely above a, never below it.
        assert!(b.score(path, NOW) >= a.score(path, NOW));
        assert!(b.score(path, NOW) < a.score(path, NOW) + 0.01);
    }

    #[test]
    fn use_fades_so_the_launcher_is_not_a_museum() {
        let path = Path::new("/apps/x.desktop");
        let mut frecency = Frecency::new();
        frecency.record(path, NOW);

        let fresh = frecency.score(path, NOW);
        let fortnight = frecency.score(path, NOW + 14 * DAY);
        let year = frecency.score(path, NOW + 365 * DAY);

        assert!(
            (fortnight - fresh / 2.0).abs() < 1e-6,
            "half-life is not a fortnight"
        );
        assert!(year < 0.01, "a year-old launch still weighs {year}");
    }

    #[test]
    fn a_recent_single_launch_outweighs_many_old_ones() {
        // What frecency is for: what you used this morning beats what you used
        // constantly last year.
        let old = Path::new("/apps/old.desktop");
        let new = Path::new("/apps/new.desktop");
        let mut frecency = Frecency::new();
        for _ in 0..20 {
            frecency.record(old, NOW - 200 * DAY);
        }
        frecency.record(new, NOW);
        assert!(frecency.score(new, NOW) > frecency.score(old, NOW));
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_inflate_a_score() {
        // NTP steps, suspend, a wrong RTC at boot. Amplifying instead of
        // decaying would pin one application to the top of the list forever.
        let path = Path::new("/apps/x.desktop");
        let mut frecency = Frecency::new();
        frecency.record(path, NOW);
        let backwards = frecency.score(path, NOW - 100 * DAY);
        assert!(
            backwards <= 1.0,
            "score grew to {backwards} when time went back"
        );
    }

    #[test]
    fn something_never_launched_scores_zero() {
        assert_eq!(Frecency::new().score(Path::new("/nope.desktop"), NOW), 0.0);
    }

    #[test]
    fn ranking_is_deterministic_for_equal_candidates() {
        // Two runs must agree, or the grid reshuffles between openings.
        let apps = apps();
        let once = search(&apps, "e", &Frecency::new(), NOW);
        let twice = search(&apps, "e", &Frecency::new(), NOW);
        assert_eq!(names(&once, &apps), names(&twice, &apps));
    }

    #[test]
    fn frecency_survives_a_trip_through_its_text_form() {
        let a = Path::new("/apps/a.desktop");
        let b = Path::new("/apps/with space.desktop");
        let mut before = Frecency::new();
        before.record(a, NOW - 3 * DAY);
        before.record(a, NOW);
        before.record(b, NOW - DAY);

        let after = Frecency::parse(&before.to_text());
        assert_eq!(after.len(), 2);
        for path in [a, b] {
            assert_eq!(after.last_used(path), before.last_used(path));
            assert!((after.score(path, NOW) - before.score(path, NOW)).abs() < 1e-9);
        }
        // And the text is the same both times, so a save never rewrites
        // bytes it does not need to.
        assert_eq!(after.to_text(), before.to_text());
    }

    #[test]
    fn malformed_lines_are_skipped_rather_than_fatal() {
        let text = "\
\n\
# a comment\n\
not a number\t1.0\t/apps/bad-time.desktop\n\
1700000000\tabc\t/apps/bad-score.desktop\n\
1700000000\t1.0\n\
1700000000\t-1.0\t/apps/negative.desktop\n\
1700000000\tNaN\t/apps/nan.desktop\n\
1700000000\t2.5\t/apps/good.desktop\r\n\
   \n";
        let frecency = Frecency::parse(text);
        assert_eq!(frecency.len(), 1, "{:?}", frecency);
        let good = Path::new("/apps/good.desktop");
        assert_eq!(frecency.last_used(good), Some(NOW));
        assert!((frecency.score(good, NOW) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn pruning_forgets_what_has_decayed_to_nothing() {
        let old = Path::new("/apps/old.desktop");
        let recent = Path::new("/apps/recent.desktop");
        let mut frecency = Frecency::new();
        frecency.record(old, NOW - 365 * DAY);
        frecency.record(recent, NOW - 20 * DAY);

        frecency.prune(NOW);
        assert_eq!(frecency.last_used(old), None, "a year-old launch was kept");
        assert!(
            frecency.last_used(recent).is_some(),
            "a recent one was lost"
        );
        assert!(!frecency.to_text().contains("old.desktop"));
    }

    #[test]
    fn saving_and_loading_go_through_the_filesystem_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "raven-frecency-test-{}-{}",
            std::process::id(),
            NOW
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // The directory does not exist yet: save must create it.
        let file = dir.join("state").join("frecency");

        assert!(
            Frecency::load(&file)
                .expect("missing is not an error")
                .is_empty(),
            "a missing file is an empty history"
        );

        let path = Path::new("/apps/x.desktop");
        let mut frecency = Frecency::new();
        frecency.record(path, NOW);
        frecency.save(&file).expect("save");

        let loaded = Frecency::load(&file).expect("load");
        assert_eq!(loaded.last_used(path), Some(NOW));

        // No temporary file left beside the real one.
        let siblings: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(siblings, vec![std::ffi::OsString::from("frecency")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subsequence_matching_respects_order() {
        // "xof" is the letters of Firefox out of order and must not match.
        assert!(is_subsequence("firefox", "frfx"));
        assert!(!is_subsequence("firefox", "xof"));
        assert!(!is_subsequence("firefox", "firefoxx"));
    }
}
