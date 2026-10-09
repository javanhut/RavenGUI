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

/// How a screen is turned, in quarter turns counter-clockwise -- the
/// numbering `wl_output.transform` uses for its first four values, so the
/// protocol and the backend can pass it through unchanged.
///
/// A monitor stood on its side is [`Self::Deg90`] or [`Self::Deg270`]
/// depending on which way it was turned; which is which is easiest found by
/// trying one, so the settings page offers both by what they look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Normal,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotation {
    /// From the protocol's number; anything past 3 is not a rotation.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Normal),
            1 => Some(Self::Deg90),
            2 => Some(Self::Deg180),
            3 => Some(Self::Deg270),
            _ => None,
        }
    }

    pub const fn raw(self) -> u32 {
        self as u32
    }

    pub const fn degrees(self) -> u32 {
        self.raw() * 90
    }

    fn from_degrees(degrees: u32) -> Option<Self> {
        if degrees.is_multiple_of(90) {
            Self::from_raw(degrees / 90)
        } else {
            None
        }
    }

    /// Whether width and height trade places: a portrait screen.
    pub const fn swaps_axes(self) -> bool {
        matches!(self, Self::Deg90 | Self::Deg270)
    }

    /// `size` as the desktop sees it once the screen is turned.
    pub const fn apply(self, size: Size) -> Size {
        if self.swaps_axes() {
            Size::new(size.h, size.w)
        } else {
            size
        }
    }
}

/// What was asked for one screen, by name.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    /// The connector it was last seen on: `eDP-1`, `HDMI-A-1`. For an entry
    /// whose monitor is not connected and whose connector has since been
    /// taken by another, its [`Self::id`] instead -- see [`rebind`].
    pub name: String,
    /// The monitor itself, from its EDID (see [`identity`]), once it has been
    /// seen. Connector names are not the monitor: two DisplayLink adapters
    /// number their `DVI-I-n` connectors in whichever order they come up, and
    /// a dock renumbers its `DP-n` from one boot to the next.
    pub id: Option<String>,
    /// Where its top-left corner goes, in logical pixels.
    pub position: Option<Point>,
    /// The effective scale to run it at instead of the one its size implies.
    pub scale: Option<f64>,
    /// Which way up it is.
    pub rotation: Rotation,
    /// Whether this is the main screen: where the shell's panels stay, new
    /// windows open, focus starts, and which is numbered 1. At most one.
    pub primary: bool,
}

impl Saved {
    /// An entry that pins nothing yet.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            id: None,
            position: None,
            scale: None,
            rotation: Rotation::Normal,
            primary: false,
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
/// where `-` is no position, optionally followed by `rotate=90` (or 180,
/// 270; counter-clockwise), `primary` and `id=` the monitor's identity.
/// Only the first `primary` in the file counts. Lines that do not parse are
/// dropped rather than failing the file: a typo in one entry should cost that
/// entry, not every screen's place. Comments start with `#`.
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
            let mut scale = None;
            let mut rotation = Rotation::Normal;
            let mut primary = false;
            let mut id = None;
            for part in parts {
                if part == "primary" {
                    primary = true;
                    continue;
                }
                if let Some(value) = part.strip_prefix("id=") {
                    id = Some(value.to_owned()).filter(|v| !v.is_empty());
                    continue;
                }
                match part.strip_prefix("rotate=") {
                    Some(degrees) => rotation = Rotation::from_degrees(degrees.parse().ok()?)?,
                    None if scale.is_none() => {
                        scale = Some(part.parse::<f64>().ok().filter(|s| *s > 0.0)?);
                    }
                    None => return None,
                }
            }
            Some(Saved {
                name: name.to_owned(),
                id,
                position,
                scale,
                rotation,
                primary,
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .scan(false, |seen, mut entry| {
            entry.primary &= !std::mem::replace(seen, *seen || entry.primary);
            Some(entry)
        })
        .collect()
}

/// Write the saved layout, in the format [`parse`] reads.
pub fn to_text(saved: &[Saved]) -> String {
    let mut out = String::from(
        "# raven outputs: name x,y [scale] [rotate=90] [primary] [id=monitor]  -- see huginn docs/outputs.md\n",
    );
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
        if entry.rotation != Rotation::Normal {
            out.push_str(&format!(" rotate={}", entry.rotation.degrees()));
        }
        if entry.primary {
            out.push_str(" primary");
        }
        if let Some(id) = &entry.id {
            out.push_str(&format!(" id={id}"));
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

/// Make `name` the main screen, or with `None` have no main screen.
pub fn set_primary(saved: &mut Vec<Saved>, name: Option<&str>) {
    for entry in saved.iter_mut() {
        entry.primary = false;
    }
    if let Some(name) = name {
        entry(saved, name).primary = true;
    }
}

/// The main screen's name, if one is set.
pub fn primary(saved: &[Saved]) -> Option<&str> {
    saved.iter().find(|s| s.primary).map(|s| s.name.as_str())
}

/// Record which way up `name` is.
pub fn set_rotation(saved: &mut Vec<Saved>, name: &str, rotation: Rotation) {
    entry(saved, name).rotation = rotation;
}

/// Which monitor an EDID describes: `HPN.36da.1CR1211QZ8`, manufacturer,
/// product code and serial number.
///
/// The serial is the one the monitor prints in its serial descriptor where
/// it has one, since the numeric field is a placeholder on plenty of them --
/// HP fill it with `0x01010101` on every unit. Without either the identity is
/// the model alone, which still tells a laptop panel from a monitor, and two
/// of the same model apart only by [`rebind`]'s fallback to the connector.
/// `None` for anything that is not an EDID.
pub fn identity(edid: &[u8]) -> Option<String> {
    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    // Three five-bit letters, 'A' as 1.
    let packed = u16::from_be_bytes([edid[8], edid[9]]);
    let maker: String = [10, 5, 0]
        .iter()
        .map(|shift| char::from(b'@' + ((packed >> shift) & 0x1f) as u8))
        .collect();
    let product = u16::from_le_bytes([edid[10], edid[11]]);
    let numeric = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);

    // The four 18-byte descriptors; 0xff is the serial number as text.
    let printed = edid[54..126]
        .as_chunks::<18>()
        .0
        .iter()
        .find(|d| d[..3] == [0, 0, 0] && d[3] == 0xff)
        .map(|d| {
            d[5..]
                .iter()
                .take_while(|b| **b != b'\n')
                .map(|b| char::from(*b))
                .collect::<String>()
                .trim()
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    let serial = printed.or_else(|| (numeric != 0).then(|| numeric.to_string()));

    let mut id = format!("{maker}.{product:04x}");
    if let Some(serial) = serial {
        id.push('.');
        id.push_str(&serial);
    }
    Some(id)
}

/// Tie the saved entries to the monitors connected now, by identity, and
/// rename each to the connector its monitor is on. Returns whether anything
/// changed, so the caller knows to save.
///
/// `connected` is every screen up right now, by connector name, with its
/// [`identity`] where its EDID gave one. After this, looking an entry up by
/// connector name -- as [`arrange`], [`primary`] and the setters all do --
/// finds the monitor's own entry whichever connector it came up on:
///
/// - An entry is found by its monitor's identity first, so two identical
///   monitors that swapped connectors overnight keep their own places.
/// - An entry with no identity yet -- one written before identities were, or
///   just set by name from Settings -- is claimed by the screen on its
///   connector, which gives it that screen's identity from then on.
/// - An entry left holding a connector name that is now some other
///   monitor's is renamed to its identity, where it waits for its monitor;
///   one with no identity is stale and dropped.
///
/// An identity two connected screens share says nothing about which is which,
/// and those two are matched by connector name as though they had none.
pub fn rebind(saved: &mut Vec<Saved>, connected: &[(String, Option<String>)]) -> bool {
    let before = saved.clone();
    let shared = |id: &str| {
        connected
            .iter()
            .filter(|(_, other)| other.as_deref() == Some(id))
            .count()
            > 1
    };
    let connected: Vec<(&str, Option<&str>)> = connected
        .iter()
        .map(|(name, id)| (name.as_str(), id.as_deref().filter(|id| !shared(id))))
        .collect();
    let present = |id: &str| connected.iter().any(|(_, other)| *other == Some(id));

    // Which connected screen each entry belongs to.
    let mut owner: Vec<Option<usize>> = vec![None; saved.len()];
    for (screen, (_, id)) in connected.iter().enumerate() {
        if let Some(id) = id
            && let Some(entry) = saved.iter().position(|s| s.id.as_deref() == Some(*id))
        {
            owner[entry] = Some(screen);
        }
    }
    for (screen, (name, id)) in connected.iter().enumerate() {
        if owner.contains(&Some(screen)) {
            continue;
        }
        // A screen with an identity claims only an entry that has none: one
        // with an identity of its own is some other monitor's. A screen whose
        // EDID said nothing takes its connector's entry unless that entry's
        // monitor is connected elsewhere.
        let claimable = |s: &Saved| match (id, s.id.as_deref()) {
            (_, None) => true,
            (None, Some(other)) => !present(other),
            (Some(_), Some(_)) => false,
        };
        if let Some(entry) = saved
            .iter()
            .enumerate()
            .position(|(i, s)| owner[i].is_none() && s.name == *name && claimable(s))
        {
            owner[entry] = Some(screen);
        }
    }

    let mut keep = vec![true; saved.len()];
    for (index, entry) in saved.iter_mut().enumerate() {
        match owner[index] {
            Some(screen) => {
                let (name, id) = connected[screen];
                name.clone_into(&mut entry.name);
                if let Some(id) = id {
                    entry.id = Some(id.to_owned());
                }
            }
            None if connected.iter().any(|(name, _)| *name == entry.name) => match &entry.id {
                Some(id) => entry.name = id.clone(),
                None => keep[index] = false,
            },
            None => {}
        }
    }
    let mut keep = keep.into_iter();
    saved.retain(|_| keep.next().unwrap_or(true));
    *saved != before
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
            position: Some(Point::new(x, y)),
            ..Saved::named(name)
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
    fn rotation_round_trips_beside_or_without_a_scale() {
        let mut saved = vec![at("DP-1", 0, 0), Saved::named("DP-2"), at("eDP-1", 0, 0)];
        set_rotation(&mut saved, "DP-1", Rotation::Deg90);
        set_scale(&mut saved, "DP-1", Some(1.5));
        set_rotation(&mut saved, "DP-2", Rotation::Deg270);
        let text = to_text(&saved);
        assert!(text.contains("DP-1 0,0 1.5 rotate=90\n"));
        assert!(text.contains("DP-2 - rotate=270\n"));
        assert!(text.contains("eDP-1 0,0\n"), "upright writes nothing extra");
        assert_eq!(parse(&text), saved);
    }

    #[test]
    fn the_main_screen_round_trips_and_there_is_only_ever_one() {
        let mut saved = vec![at("eDP-1", 0, 0), at("DP-1", 1920, 0)];
        set_primary(&mut saved, Some("DP-1"));
        assert_eq!(primary(&saved), Some("DP-1"));
        let text = to_text(&saved);
        assert!(text.contains("DP-1 1920,0 primary\n"));
        assert_eq!(parse(&text), saved);

        set_primary(&mut saved, Some("eDP-1"));
        assert_eq!(primary(&saved), Some("eDP-1"));
        assert!(!saved[1].primary, "the old one gave it up");
        set_primary(&mut saved, Some("HDMI-A-1"));
        assert_eq!(saved.len(), 3, "a screen not seen yet can be main");
        set_primary(&mut saved, None);
        assert_eq!(primary(&saved), None);

        let two = parse("DP-1 0,0 primary\neDP-1 - 1.5 rotate=90 primary\n");
        assert!(
            two[0].primary && !two[1].primary,
            "a hand-edited second one is ignored"
        );
        assert_eq!(two[1].rotation, Rotation::Deg90);
    }

    #[test]
    fn a_rotation_that_is_not_a_quarter_turn_costs_its_line() {
        let parsed = parse(
            "DP-1 0,0 rotate=45\nDP-2 0,0 rotate=sideways\nDP-3 0,0 1 2\neDP-1 - rotate=180\n",
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].rotation, Rotation::Deg180);
    }

    #[test]
    fn a_turned_screen_is_placed_at_its_turned_size() {
        let portrait = Rotation::Deg90.apply(Size::new(2560, 1440));
        let placed = arrange(
            &[
                screen("eDP-1", 1920, 1080, true),
                screen("DP-1", portrait.w, portrait.h, false),
            ],
            &[],
        );
        assert_eq!(placed[1], Rect::from_xywh(1920, 0, 1440, 2560));
    }

    fn bound(name: &str, id: &str, x: i32, y: i32) -> Saved {
        Saved {
            id: Some(id.into()),
            ..at(name, x, y)
        }
    }

    fn up(screens: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        screens
            .iter()
            .map(|(name, id)| ((*name).to_owned(), id.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn two_identical_monitors_that_swap_connectors_keep_their_own_places() {
        // Two DisplayLink adapters came up the other way round this boot.
        let mut saved = vec![
            Saved {
                rotation: Rotation::Deg90,
                ..bound("DVI-I-1", "HPN.36da.QZ8", 0, 0)
            },
            Saved {
                primary: true,
                ..bound("DVI-I-2", "HPN.36da.QZB", 1080, 0)
            },
        ];
        let changed = rebind(
            &mut saved,
            &up(&[
                ("DVI-I-1", Some("HPN.36da.QZB")),
                ("DVI-I-2", Some("HPN.36da.QZ8")),
            ]),
        );
        assert!(changed);
        let qz8 = saved.iter().find(|s| s.name == "DVI-I-2").unwrap();
        assert_eq!(qz8.id.as_deref(), Some("HPN.36da.QZ8"));
        assert_eq!(
            qz8.rotation,
            Rotation::Deg90,
            "the turned one is still turned"
        );
        assert_eq!(qz8.position, Some(Point::new(0, 0)));
        assert_eq!(primary(&saved), Some("DVI-I-1"), "main follows the monitor");
    }

    #[test]
    fn an_entry_from_before_identities_is_claimed_by_its_connector() {
        let mut saved = vec![at("HDMI-A-1", 1920, 0)];
        assert!(rebind(
            &mut saved,
            &up(&[("HDMI-A-1", Some("HPN.366c.3CM"))])
        ));
        assert_eq!(saved[0].id.as_deref(), Some("HPN.366c.3CM"));
        assert!(
            !rebind(&mut saved, &up(&[("HDMI-A-1", Some("HPN.366c.3CM"))])),
            "settled: nothing to save the second time"
        );
    }

    #[test]
    fn an_absent_monitor_gives_up_its_connector_name_and_waits() {
        let mut saved = vec![bound("DP-1", "DEL.a0b1.X", -1920, 0)];
        // Another monitor on the same port.
        rebind(&mut saved, &up(&[("DP-1", Some("GSM.5b7f.Y"))]));
        assert_eq!(saved.len(), 1, "a new monitor makes no entry of its own");
        assert_eq!(saved[0].name, "DEL.a0b1.X");
        assert!(
            arrange(&[screen("DP-1", 1920, 1080, false)], &saved)[0].x() == 0,
            "the stranger is not put where the absent one was"
        );
        // And it comes back.
        rebind(&mut saved, &up(&[("DP-3", Some("DEL.a0b1.X"))]));
        assert_eq!(saved[0].name, "DP-3");
        assert_eq!(saved[0].position, Some(Point::new(-1920, 0)));
    }

    #[test]
    fn a_stale_entry_with_no_identity_is_dropped_when_its_connector_is_taken() {
        let mut saved = vec![at("DVI-I-1", 0, 0), bound("DVI-I-2", "A", 1920, 0)];
        // A's monitor moved to DVI-I-1; the old unnamed DVI-I-1 entry is
        // nobody's now.
        rebind(&mut saved, &up(&[("DVI-I-1", Some("A"))]));
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "DVI-I-1");
        assert_eq!(saved[0].position, Some(Point::new(1920, 0)));
    }

    #[test]
    fn a_shared_identity_falls_back_to_connector_names() {
        let mut saved = vec![
            bound("DP-1", "X.0001", 0, 0),
            bound("DP-2", "X.0001", 1920, 0),
        ];
        rebind(
            &mut saved,
            &up(&[("DP-1", Some("X.0001")), ("DP-2", Some("X.0001"))]),
        );
        assert_eq!(saved[0].name, "DP-1");
        assert_eq!(saved[1].name, "DP-2");
    }

    #[test]
    fn the_identity_round_trips() {
        let saved = vec![Saved {
            primary: true,
            ..bound("DVI-I-1", "HPN.36da.1CR1211QZ8", 1, 0)
        }];
        let text = to_text(&saved);
        assert!(text.contains("DVI-I-1 1,0 primary id=HPN.36da.1CR1211QZ8\n"));
        assert_eq!(parse(&text), saved);
    }

    fn edid(maker: &str, product: u16, numeric: u32, printed: Option<&str>) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        let m: Vec<u16> = maker.bytes().map(|b| u16::from(b - b'@')).collect();
        e[8..10].copy_from_slice(&((m[0] << 10) | (m[1] << 5) | m[2]).to_be_bytes());
        e[10..12].copy_from_slice(&product.to_le_bytes());
        e[12..16].copy_from_slice(&numeric.to_le_bytes());
        if let Some(printed) = printed {
            let d = &mut e[72..90];
            d[3] = 0xff;
            // Thirteen bytes: a newline ends a shorter one, and one that
            // fills the field has none.
            let mut text = printed.as_bytes().to_vec();
            text.push(b'\n');
            text.truncate(13);
            d[5..5 + text.len()].copy_from_slice(&text);
        }
        e
    }

    #[test]
    fn an_edid_names_its_monitor() {
        assert_eq!(
            identity(&edid("HPN", 14042, 0x0101_0101, Some("1CR1211QZ8"))).as_deref(),
            Some("HPN.36da.1CR1211QZ8"),
            "the printed serial wins over the placeholder"
        );
        assert_eq!(
            identity(&edid("HPN", 13932, 0x0101_0101, Some("3CM2120B3F   "))).as_deref(),
            Some("HPN.366c.3CM2120B3F"),
            "padding trimmed"
        );
        assert_eq!(
            identity(&edid("CMN", 5046, 0, None)).as_deref(),
            Some("CMN.13b6"),
            "a laptop panel with no serial is its model"
        );
        assert_eq!(
            identity(&edid("DEL", 1, 42, None)).as_deref(),
            Some("DEL.0001.42")
        );
        assert_eq!(identity(&[0; 128]), None);
        assert_eq!(identity(&[]), None);
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
