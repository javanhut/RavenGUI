//! Frame-time percentiles, per screen.
//!
//! "Smooth" is a p99, not an average: a desktop that renders in 3 ms on
//! average and 40 ms once a second is a desktop that stutters once a second.
//! So each screen keeps two samples per frame -- how long the compositor
//! took to build and submit it (`render`), and how long the page flip then
//! took to land on the panel (`present`) -- and once a minute the
//! percentiles of the frames drawn in that minute go to the log and to a
//! file under the runtime directory, where `cat` can read them without a
//! debugger and without asking the compositor for anything. A minute with
//! no frames logs nothing: an idle desktop is not news.
//!
//! The samples are kept only until they are reported, bounded so a runaway
//! frame rate cannot grow memory, and the cost per frame is two pushes.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

/// Samples kept between reports, at most. A minute at 60 Hz is 3600; more
/// than that is a screen at 144 Hz or a report that was late, and the
/// oldest go first.
const CAP: usize = 8192;

/// One screen's samples since the last report, plus what has been drawn
/// since the compositor started.
#[derive(Debug, Default)]
pub(crate) struct FrameStats {
    render_ms: VecDeque<f32>,
    present_ms: VecDeque<f32>,
    /// Frames rendered that turned out to have nothing new on screen, so no
    /// flip was queued. Cheap, but a high count on an idle desktop means
    /// something is marking the screen dirty for no reason.
    skipped: u32,
    pub(crate) frames_total: u64,
    pub(crate) skipped_total: u64,
}

/// The percentiles of one kind of sample over one report interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Percentiles {
    pub(crate) count: usize,
    pub(crate) p50: f32,
    pub(crate) p99: f32,
    pub(crate) max: f32,
}

/// What one screen reports for one interval.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Report {
    pub(crate) frames: usize,
    pub(crate) skipped: u32,
    pub(crate) render: Option<Percentiles>,
    pub(crate) present: Option<Percentiles>,
}

impl FrameStats {
    fn push(deque: &mut VecDeque<f32>, ms: f32) {
        if deque.len() == CAP {
            deque.pop_front();
        }
        deque.push_back(ms);
    }

    /// A frame was built and its flip queued; `took` is the build time.
    pub(crate) fn record_render(&mut self, took: Duration) {
        Self::push(&mut self.render_ms, took.as_secs_f32() * 1000.0);
        self.frames_total += 1;
    }

    /// The flip queued `after` ago has landed on the panel.
    pub(crate) fn record_present(&mut self, after: Duration) {
        Self::push(&mut self.present_ms, after.as_secs_f32() * 1000.0);
    }

    /// A frame was built and had nothing new in it.
    pub(crate) fn record_skipped(&mut self) {
        self.skipped += 1;
        self.skipped_total += 1;
    }

    /// Whether anything happened since the last report.
    pub(crate) fn is_quiet(&self) -> bool {
        self.render_ms.is_empty() && self.present_ms.is_empty() && self.skipped == 0
    }

    /// The interval's report, and the start of the next interval.
    pub(crate) fn take_report(&mut self) -> Report {
        let report = Report {
            frames: self.render_ms.len(),
            skipped: self.skipped,
            render: percentiles(self.render_ms.iter().copied()),
            present: percentiles(self.present_ms.iter().copied()),
        };
        self.render_ms.clear();
        self.present_ms.clear();
        self.skipped = 0;
        report
    }
}

/// Nearest-rank percentiles. `None` for no samples.
pub(crate) fn percentiles(samples: impl Iterator<Item = f32>) -> Option<Percentiles> {
    let mut v: Vec<f32> = samples.collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let rank = |p: f32| -> f32 {
        let idx = ((p / 100.0) * n as f32).ceil() as usize;
        v[idx.clamp(1, n) - 1]
    };
    Some(Percentiles {
        count: n,
        p50: rank(50.0),
        p99: rank(99.0),
        max: v[n - 1],
    })
}

/// A millisecond value rounded to a tenth, for log fields that should read
/// as numbers rather than as formatted text.
pub(crate) fn tenths(ms: f32) -> f32 {
    (ms * 10.0).round() / 10.0
}

/// The file the latest report is written to, if there is a runtime dir.
pub(crate) fn report_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join("huginn").join("frametime"))
}

/// Render one interval's reports as the text the runtime file holds.
pub(crate) fn render_text(interval: Duration, rows: &[(String, Report)]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "huginn frame times, last {}s, milliseconds; nearest-rank percentiles",
        interval.as_secs()
    );
    let _ = writeln!(
        out,
        "{:<12} {:>6}  {:>10} {:>7} {:>7}  {:>11} {:>7} {:>7}  {:>7}",
        "SCREEN", "FRAMES", "RENDER p50", "p99", "max", "PRESENT p50", "p99", "max", "SKIPPED"
    );
    let cell = |p: Option<Percentiles>, f: fn(&Percentiles) -> f32| -> String {
        p.map(|p| format!("{:.1}", f(&p)))
            .unwrap_or_else(|| "-".to_string())
    };
    for (name, r) in rows {
        let _ = writeln!(
            out,
            "{:<12} {:>6}  {:>10} {:>7} {:>7}  {:>11} {:>7} {:>7}  {:>7}",
            name,
            r.frames,
            cell(r.render, |p| p.p50),
            cell(r.render, |p| p.p99),
            cell(r.render, |p| p.max),
            cell(r.present, |p| p.p50),
            cell(r.present, |p| p.p99),
            cell(r.present, |p| p.max),
            r.skipped
        );
    }
    out
}

/// Write the runtime file, whole and by rename, so a reader never sees a
/// half-written report. Errors are the caller's to log; a missing runtime
/// dir is not an error, it is a session without one.
pub(crate) fn write_report(text: &str) -> std::io::Result<()> {
    let Some(path) = report_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: &[f32]) -> Vec<f32> {
        v.to_vec()
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let p = percentiles(ms(&[5.0, 1.0, 3.0, 2.0, 4.0]).into_iter()).unwrap();
        assert_eq!(p.count, 5);
        assert_eq!(p.p50, 3.0);
        assert_eq!(p.p99, 5.0);
        assert_eq!(p.max, 5.0);
        // Nearest rank: p99 of a hundred is the 99th smallest, so one slow
        // frame in a hundred shows in max and not yet in p99...
        let mut hundred: Vec<f32> = vec![3.0; 99];
        hundred.push(40.0);
        let p = percentiles(hundred.into_iter()).unwrap();
        assert_eq!(p.p50, 3.0);
        assert_eq!(p.p99, 3.0);
        assert_eq!(p.max, 40.0);
        // ...and more than one in a hundred is exactly what p99 exists to show.
        let mut thousand: Vec<f32> = vec![3.0; 985];
        thousand.extend(std::iter::repeat_n(40.0, 15));
        let p = percentiles(thousand.into_iter()).unwrap();
        assert_eq!(p.p50, 3.0);
        assert_eq!(p.p99, 40.0);
        assert!(percentiles(std::iter::empty()).is_none());
    }

    #[test]
    fn a_report_drains_the_interval_and_keeps_totals() {
        let mut s = FrameStats::default();
        assert!(s.is_quiet());
        s.record_render(Duration::from_millis(4));
        s.record_present(Duration::from_millis(16));
        s.record_skipped();
        assert!(!s.is_quiet());
        let r = s.take_report();
        assert_eq!(r.frames, 1);
        assert_eq!(r.skipped, 1);
        assert_eq!(r.render.unwrap().p50, 4.0);
        assert_eq!(r.present.unwrap().max, 16.0);
        assert!(s.is_quiet(), "the interval starts empty");
        assert_eq!(s.frames_total, 1);
        assert_eq!(s.skipped_total, 1);
        let r = s.take_report();
        assert_eq!(r.frames, 0);
        assert!(r.render.is_none());
    }

    #[test]
    fn samples_are_bounded() {
        let mut s = FrameStats::default();
        for i in 0..(CAP + 100) {
            s.record_render(Duration::from_micros(i as u64));
        }
        let r = s.take_report();
        assert_eq!(r.frames, CAP);
        // The oldest went first, so the smallest surviving sample is the
        // 101st recorded.
        assert!(r.render.unwrap().p50 >= 0.1);
        assert_eq!(s.frames_total as usize, CAP + 100);
    }

    #[test]
    fn text_has_one_row_per_screen() {
        let rows = vec![
            (
                "eDP-1".to_string(),
                Report {
                    frames: 312,
                    skipped: 4,
                    render: Some(Percentiles { count: 312, p50: 3.1, p99: 7.8, max: 12.4 }),
                    present: Some(Percentiles { count: 312, p50: 16.6, p99: 17.2, max: 33.1 }),
                },
            ),
            (
                "HDMI-A-1".to_string(),
                Report { frames: 0, skipped: 0, render: None, present: None },
            ),
        ];
        let text = render_text(Duration::from_secs(60), &rows);
        assert!(text.contains("last 60s"));
        assert!(text.lines().any(|l| l.starts_with("eDP-1") && l.contains("3.1") && l.contains("7.8")));
        assert!(text.lines().any(|l| l.starts_with("HDMI-A-1") && l.contains(" - ")));
    }
}
