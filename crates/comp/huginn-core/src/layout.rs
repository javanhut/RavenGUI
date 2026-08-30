//! Where each screen sits in the desktop, and how that is remembered.
//!
//! Two questions, kept apart. [`Saved`] is what the person asked for: a
//! position for a named screen, perhaps a scale, written down once and kept
//! across sessions in the format [`parse`] and [`to_text`] agree on. [`arrange`]
//! is what happens when the screens actually connected meet that wish list:
//! a saved position is honoured where it can be, a screen nobody has placed
//! goes to the right of everything, and nothing is allowed to overlap, because
//! two screens over the same desktop pixels is a desktop you cannot reach.
//!
//! Pure functions over names and rectangles, so every case -- a monitor that
//! was placed above the laptop coming back after a week, two saved positions
//! that collide, a saved entry for a screen that is not plugged in -- is a
//! test rather than a thing to discover on a Monday.

use crate::geometry::{Point, Rect, Size};

/// A screen that is connected right now, before it has a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The connector name: `eDP-1`, `HDMI-A-1`.
    pub name: String,
    /// Its size in logical pixels at the scale it will run at.
    pub size: Size,
    /// Whether it is the machine's own panel, which anchors the desktop when
    /// nothing has been placed by hand.
    pub builtin: bool,
}

/// What was asked for one screen, by name.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub name: String,
    /// Where its top-left corner goes, in logical pixels.
    pub position: Option<Point>,
    /// The effective scale to run it at instead of the one its size implies.
    pub scale: Option<f64>,
}

impl Saved {
    /// An entry that pins nothing yet.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            position: None,
            scale: None,
        }
    }
}

/// Place the connected screens, honouring saved positions.
///
/// Returns one rectangle per candidate, in the same order. Screens with a
/// saved position are placed first, in reading order of those positions, so
/// that when two collide the one further up and left keeps its place and the
/// other is pushed right of it. The rest follow, built-in panel first and
/// then by name, each to the right of everything placed so far. Finally the
/// whole arrangement is shifted so its top-left is the origin: clients see
/// the same shape either way, and a desktop that starts at (0, 0) is the one
/// every X11 assumption was written against.
pub fn arrange(candidates: &[Candidate], saved: &[Saved]) -> Vec<Rect> {
    let position_of = |name: &str| {
        saved
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.position)
    };

    // Placement order, as indices into `candidates`.
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| {
        let (ca, cb) = (&candidates[a], &candidates[b]);
        let (pa, pb) = (position_of(&ca.name), position_of(&cb.name));
        match (pa, pb) {
            (Some(pa), Some(pb)) => (pa.y, pa.x).cmp(&(pb.y, pb.x)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => cb
                .builtin
                .cmp(&ca.builtin)
                .then_with(|| ca.name.cmp(&cb.name)),
        }
    });

    let mut placed: Vec<Rect> = vec![Rect::ZERO; candidates.len()];
    let mut taken: Vec<Rect> = Vec::new();
    for index in order {
        let candidate = &candidates[index];
        let rect = match position_of(&candidate.name) {
            Some(at) => {
                let wanted = Rect::new(at, candidate.size);
                if taken.iter().any(|t| t.overlaps(wanted)) {
                    // Keep the row it asked for; give up the column.
                    right_of(&taken, candidate.size, at.y)
                } else {
                    wanted
                }
            }
            None => right_of(&taken, candidate.size, 0),
        };
        taken.push(rect);
        placed[index] = rect;
    }

    // Normalise to the origin.
    let min_x = placed.iter().map(|r| r.x()).min().unwrap_or(0);
    let min_y = placed.iter().map(|r| r.y()).min().unwrap_or(0);
    for rect in &mut placed {
        *rect = Rect::from_xywh(rect.x() - min_x, rect.y() - min_y, rect.w(), rect.h());
    }
    placed
}

/// A rectangle of `size` at row `y`, right of everything in `taken`.
fn right_of(taken: &[Rect], size: Size, y: i32) -> Rect {
    let x = taken.iter().map(|r| r.right()).max().unwrap_or(0);
    Rect::from_xywh(x, y, size.w, size.h)
}

/// Read the saved layout.
///
/// One screen per line: `name x,y` or `name x,y scale` or `name - scale`,
/// where `-` is no position. Lines that do not parse are dropped rather than
/// failing the file: a typo in one entry should cost that entry, not every
/// screen's place. Comments start with `#`.
pub fn parse(text: &str) -> Vec<Saved> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let position = match parts.next()? {
                "-" => None,
                at => {
                    let (x, y) = at.split_once(',')?;
                    Some(Point::new(x.parse().ok()?, y.parse().ok()?))
                }
            };
            let scale = match parts.next() {
                None => None,
                Some(s) => Some(s.parse::<f64>().ok().filter(|s| *s > 0.0)?),
            };
            Some(Saved {
                name: name.to_owned(),
                position,
                scale,
            })
        })
        .collect()
}

/// Write the saved layout, in the format [`parse`] reads.
pub fn to_text(saved: &[Saved]) -> String {
    let mut out =
        String::from("# raven outputs: name x,y [scale]  -- see huginn docs/outputs.md\n");
    for entry in saved {
        out.push_str(&entry.name);
        out.push(' ');
        match entry.position {
            Some(at) => out.push_str(&format!("{},{}", at.x, at.y)),
            None => out.push('-'),
        }
        if let Some(scale) = entry.scale {
            out.push_str(&format!(" {scale}"));
        }
        out.push('\n');
    }
    out
}

/// Record a position for `name`, keeping whatever else was saved for it.
pub fn set_position(saved: &mut Vec<Saved>, name: &str, at: Point) {
    entry(saved, name).position = Some(at);
}

/// Record a scale for `name`, or clear it with `None`.
pub fn set_scale(saved: &mut Vec<Saved>, name: &str, scale: Option<f64>) {
    entry(saved, name).scale = scale;
}

fn entry<'a>(saved: &'a mut Vec<Saved>, name: &str) -> &'a mut Saved {
    if let Some(index) = saved.iter().position(|s| s.name == name) {
        &mut saved[index]
    } else {
        saved.push(Saved::named(name));
        saved.last_mut().expect("just pushed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(name: &str, w: i32, h: i32, builtin: bool) -> Candidate {
        Candidate {
            name: name.into(),
            size: Size::new(w, h),
            builtin,
        }
    }

    fn at(name: &str, x: i32, y: i32) -> Saved {
        Saved {
            name: name.into(),
            position: Some(Point::new(x, y)),
            scale: None,
        }
    }

    #[test]
    fn with_nothing_saved_the_builtin_panel_anchors_and_the_rest_go_right() {
        let placed = arrange(
            &[
                screen("HDMI-A-1", 2560, 1440, false),
                screen("eDP-1", 1920, 1080, true),
            ],
            &[],
        );
        assert_eq!(
            placed[1],
            Rect::from_xywh(0, 0, 1920, 1080),
            "the panel is first"
        );
        assert_eq!(placed[0], Rect::from_xywh(1920, 0, 2560, 1440));
    }

    #[test]
    fn a_saved_position_is_honoured() {
        // The monitor above the laptop, as a client that arranges the whole
        // desktop saves it: every connected screen gets a position.
        let placed = arrange(
            &[
                screen("eDP-1", 1920, 1080, true),
                screen("HDMI-A-1", 2560, 1440, false),
            ],
            &[at("HDMI-A-1", 0, -1440), at("eDP-1", 0, 0)],
        );
        // Normalised: the monitor is at the origin and the panel below it.
        assert_eq!(placed[1], Rect::from_xywh(0, 0, 2560, 1440));
        assert_eq!(placed[0], Rect::from_xywh(0, 1440, 1920, 1080));
    }

    #[test]
    fn a_screen_saved_off_the_origin_leaves_an_unsaved_one_beside_it() {
        // Only the monitor was ever placed; the panel has no entry and
        // follows the rule for unplaced screens, beside it rather than under.
        let placed = arrange(
            &[
                screen("eDP-1", 1920, 1080, true),
                screen("HDMI-A-1", 2560, 1440, false),
            ],
            &[at("HDMI-A-1", 0, -1440)],
        );
        assert_eq!(placed[1], Rect::from_xywh(0, 0, 2560, 1440));
        assert!(!placed[0].overlaps(placed[1]));
        assert_eq!(placed[0].x(), 2560);
    }

    #[test]
    fn a_monitor_to_the_left_of_the_panel_stays_left() {
        let placed = arrange(
            &[
                screen("eDP-1", 1920, 1080, true),
                screen("DP-1", 1920, 1080, false),
            ],
            &[at("DP-1", -1920, 0), at("eDP-1", 0, 0)],
        );
        assert_eq!(placed[1], Rect::from_xywh(0, 0, 1920, 1080));
        assert_eq!(placed[0], Rect::from_xywh(1920, 0, 1920, 1080));
    }

    #[test]
    fn colliding_saved_positions_push_the_later_one_right() {
        let placed = arrange(
            &[
                screen("DP-1", 1920, 1080, false),
                screen("DP-2", 1920, 1080, false),
            ],
            &[at("DP-1", 0, 0), at("DP-2", 100, 0)],
        );
        assert_eq!(placed[0], Rect::from_xywh(0, 0, 1920, 1080));
        assert_eq!(
            placed[1],
            Rect::from_xywh(1920, 0, 1920, 1080),
            "moved off DP-1"
        );
        assert!(!placed[0].overlaps(placed[1]));
    }

    #[test]
    fn a_saved_entry_for_an_absent_screen_changes_nothing() {
        let placed = arrange(
            &[screen("eDP-1", 1920, 1080, true)],
            &[at("HDMI-A-1", 5000, 5000)],
        );
        assert_eq!(placed, vec![Rect::from_xywh(0, 0, 1920, 1080)]);
    }

    #[test]
    fn unsaved_screens_go_right_of_the_saved_ones() {
        let placed = arrange(
            &[
                screen("eDP-1", 1920, 1080, true),
                screen("HDMI-A-1", 2560, 1440, false),
            ],
            &[at("HDMI-A-1", 0, 0)],
        );
        assert_eq!(placed[1], Rect::from_xywh(0, 0, 2560, 1440));
        assert_eq!(placed[0], Rect::from_xywh(2560, 0, 1920, 1080));
    }

    #[test]
    fn the_file_round_trips() {
        let mut saved = vec![at("HDMI-A-1", 0, -1440), Saved::named("eDP-1")];
        set_scale(&mut saved, "eDP-1", Some(1.25));
        set_position(&mut saved, "eDP-1", Point::new(0, 0));
        let text = to_text(&saved);
        assert_eq!(parse(&text), saved);
    }

    #[test]
    fn a_bad_line_costs_only_itself() {
        let parsed =
            parse("# comment\nHDMI-A-1 0,-1440\nDP-1 garbage\neDP-1 - 1.5\nDP-2 0,0 zero\n");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].position, Some(Point::new(0, -1440)));
        assert_eq!(parsed[1].name, "eDP-1");
        assert_eq!(parsed[1].position, None);
        assert_eq!(parsed[1].scale, Some(1.5));
    }

    #[test]
    fn setting_a_scale_keeps_the_position() {
        let mut saved = vec![at("DP-1", 10, 20)];
        set_scale(&mut saved, "DP-1", Some(2.0));
        assert_eq!(saved[0].position, Some(Point::new(10, 20)));
        set_scale(&mut saved, "DP-1", None);
        assert_eq!(saved[0].scale, None);
        set_position(&mut saved, "NEW", Point::new(1, 1));
        assert_eq!(saved.len(), 2);
    }
}
