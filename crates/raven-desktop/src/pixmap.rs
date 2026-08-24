//! Icon files turned into pixels, and kept.
//!
//! [`icon`](crate::icon) resolves a name to a path; this reads that path and
//! rasterizes it at the size actually wanted. Both formats a real icon theme
//! ships are handled — SVG (and `.svgz`) through resvg, PNG through tiny-skia —
//! because a launcher cannot choose which one an application installed. The
//! RavenTerminal icon is SVG at *every* size, with no raster fallback at all,
//! so an implementation that only read PNG would show nothing for it.
//!
//! # Why the cache exists
//!
//! Rasterizing an SVG costs on the order of a millisecond. The launcher redraws
//! on every keystroke and shows eight rows, so an uncached path would rasterize
//! eight icons per character typed — tens of milliseconds of the 400ms budget,
//! spent redoing work that could not have changed.
//!
//! # Why it is keyed on more than the path
//!
//! A package manager replaces theme files in place. Keyed on path alone, a
//! theme upgrade would leave the old icons on screen until the session
//! restarted, which looks like the upgrade did not work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use resvg::tiny_skia;
use resvg::usvg;

/// An icon rasterized to premultiplied RGBA, ready to blend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    /// Premultiplied RGBA, four bytes per pixel, row-major.
    ///
    /// Premultiplied because that is what both rasterizers produce and what
    /// Wayland expects; un-premultiplying to blend and re-multiplying to store
    /// would lose precision at every partially-transparent edge, which on an
    /// icon is most of its outline.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Pixmap {
    /// The pixel at `x`, `y` as premultiplied `[r, g, b, a]`.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = ((y * self.width + x) * 4) as usize;
        self.data.get(at..at + 4)?.try_into().ok()
    }
}

/// What a cache entry was keyed on.
///
/// Size is part of it because the same icon at 24 and 48 pixels are different
/// images, and an SVG rendered at one and scaled to the other is exactly the
/// blur that rendering vectors at final size exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    size: u32,
    /// Modification time in nanoseconds, and length. Together these catch a
    /// theme replaced in place, which a path alone would not.
    stamp: (u128, u64),
}

/// Rasterized icons, kept between draws.
#[derive(Debug, Default)]
pub struct Pixmaps {
    cache: HashMap<Key, Option<Pixmap>>,
}

impl Pixmaps {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are held.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Drop everything. For a theme change, or to bound memory.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// The icon at `path`, rasterized to `size` square.
    ///
    /// `None` when the file cannot be read or is not an image this understands.
    /// Failures are cached too: a launcher that retried a broken icon on every
    /// keystroke would pay for the failure over and over, and the answer would
    /// not change until the file did.
    pub fn get(&mut self, path: &Path, size: u32) -> Option<&Pixmap> {
        let key = Key {
            path: path.to_owned(),
            size,
            stamp: stamp(path),
        };
        self.cache
            .entry(key)
            .or_insert_with(|| rasterize(path, size))
            .as_ref()
    }
}

/// Modification time in nanoseconds and file length, or zeroes if unavailable.
///
/// Nanoseconds rather than seconds because second granularity genuinely is not
/// enough: two files written in the same second that happen to be the same
/// length compare equal, and the cache then serves the old icon forever. That
/// is not hypothetical — the two fixtures in this module's tests are the same
/// length, and second-granularity stamps made the invalidation test fail.
///
/// A missing mtime falls back to zeroes rather than refusing to draw: an icon
/// on a filesystem that reports no times is still an icon.
fn stamp(path: &Path) -> (u128, u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, 0);
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    (modified, meta.len())
}

/// Read and rasterize one icon file.
fn rasterize(path: &Path, size: u32) -> Option<Pixmap> {
    if size == 0 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;

    // Sniffed from the content rather than trusted from the extension: an
    // icon theme is a directory of files installed by other people, and a
    // `.png` that is really an SVG is a thing that happens.
    let pixmap = if bytes.starts_with(b"\x89PNG") {
        from_png(&bytes, size)
    } else {
        from_svg(&bytes, size)
    }?;

    Some(Pixmap {
        data: pixmap.data().to_vec(),
        width: pixmap.width(),
        height: pixmap.height(),
    })
}

/// Rasterize an SVG at exactly `size`, preserving its aspect ratio.
fn from_svg(bytes: &[u8], size: u32) -> Option<tiny_skia::Pixmap> {
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let source = tree.size();
    if source.width() <= 0.0 || source.height() <= 0.0 {
        return None;
    }

    // Fit inside the square and centre what is left over. Icons are not all
    // square — the RavenTerminal icon has a portrait viewBox — and scaling
    // each axis independently to fill the box would stretch it.
    let scale = (size as f32 / source.width()).min(size as f32 / source.height());
    let (w, h) = (source.width() * scale, source.height() * scale);
    let transform = tiny_skia::Transform::from_translate(
        (size as f32 - w) / 2.0,
        (size as f32 - h) / 2.0,
    )
    .pre_scale(scale, scale);

    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

/// Decode a PNG and scale it to `size`.
fn from_png(bytes: &[u8], size: u32) -> Option<tiny_skia::Pixmap> {
    let source = tiny_skia::Pixmap::decode_png(bytes).ok()?;
    if source.width() == 0 || source.height() == 0 {
        return None;
    }
    if source.width() == size && source.height() == size {
        return Some(source);
    }

    let scale = (size as f32 / source.width() as f32).min(size as f32 / source.height() as f32);
    let (w, h) = (source.width() as f32 * scale, source.height() as f32 * scale);
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    pixmap.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &tiny_skia::PixmapPaint {
            // Bilinear rather than nearest: a raster icon being resized is
            // already a compromise, and the nearest-neighbour version of that
            // compromise looks like a mistake.
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        tiny_skia::Transform::from_translate((size as f32 - w) / 2.0, (size as f32 - h) / 2.0)
            .pre_scale(scale, scale),
        None,
    );
    Some(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch file that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("raven-pixmap-{name}"));
            std::fs::write(&path, bytes).expect("scratch file");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A blue square, as SVG.
    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
        <rect width="10" height="10" fill="#0000ff"/></svg>"##;

    /// Twice as tall as it is wide, so aspect handling is visible.
    const PORTRAIT: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 20">
        <rect width="10" height="20" fill="#00ff00"/></svg>"##;

    #[test]
    fn an_svg_rasterizes_to_the_size_asked_for() {
        let f = Scratch::new("square.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        let pixmap = cache.get(&f.0, 48).expect("rasterizes");
        assert_eq!((pixmap.width, pixmap.height), (48, 48));
        assert_eq!(pixmap.data.len(), 48 * 48 * 4);
    }

    #[test]
    fn the_pixels_are_actually_the_icon() {
        let f = Scratch::new("blue.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        let pixmap = cache.get(&f.0, 32).expect("rasterizes");
        let [r, g, b, a] = pixmap.pixel(16, 16).expect("centre pixel");
        assert!(b > 200 && r < 60 && g < 60, "centre was [{r},{g},{b},{a}]");
        assert_eq!(a, 255, "the square should be opaque");
    }

    #[test]
    fn a_non_square_icon_keeps_its_shape() {
        // Stretching each axis to fill the box is the obvious implementation
        // and it distorts every icon that is not square — which includes the
        // one this project ships.
        let f = Scratch::new("portrait.svg", PORTRAIT.as_bytes());
        let mut cache = Pixmaps::new();
        let pixmap = cache.get(&f.0, 40).expect("rasterizes");
        assert_eq!((pixmap.width, pixmap.height), (40, 40), "canvas is square");

        // Twice as tall as wide, fitted into 40x40, is 20 wide and centred —
        // so the far left and right columns are empty and the centre is not.
        assert_eq!(pixmap.pixel(1, 20).expect("left edge")[3], 0, "left edge is not clear");
        assert_eq!(pixmap.pixel(38, 20).expect("right edge")[3], 0, "right edge is not clear");
        assert!(pixmap.pixel(20, 20).expect("centre")[3] > 0, "centre is empty");
    }

    #[test]
    fn rasterizing_twice_uses_the_cache() {
        let f = Scratch::new("cached.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        assert!(cache.is_empty());
        cache.get(&f.0, 32);
        assert_eq!(cache.len(), 1);
        cache.get(&f.0, 32);
        assert_eq!(cache.len(), 1, "the second lookup added an entry");
    }

    #[test]
    fn each_size_is_cached_separately() {
        // An SVG rendered at 24 and scaled to 48 is the blur that rendering at
        // final size exists to avoid, so the sizes cannot share an entry.
        let f = Scratch::new("sizes.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        assert_eq!(cache.get(&f.0, 24).expect("24").width, 24);
        assert_eq!(cache.get(&f.0, 48).expect("48").width, 48);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn replacing_the_file_invalidates_its_entry() {
        // A package manager replaces theme files in place. Keyed on path
        // alone, a theme upgrade would show the old icons until logout.
        let f = Scratch::new("replaced.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        // A blue square fills the canvas; the green portrait is fitted, so
        // its left edge is clear. One pixel tells them apart without printing
        // two whole images on failure.
        let before = cache.get(&f.0, 16).expect("first").pixel(1, 8).expect("edge");

        std::fs::write(&f.0, PORTRAIT.as_bytes()).expect("replace");
        let after = cache.get(&f.0, 16).expect("second").pixel(1, 8).expect("edge");
        assert_ne!(
            before, after,
            "the stale icon survived the file being replaced"
        );
    }

    #[test]
    fn a_missing_file_is_none_rather_than_a_panic() {
        let mut cache = Pixmaps::new();
        assert!(cache.get(Path::new("/nonexistent/icon.svg"), 32).is_none());
    }

    #[test]
    fn a_file_that_is_not_an_image_is_none() {
        // /usr/share/icons is a directory of files installed by other people.
        let f = Scratch::new("garbage.svg", b"this is not an svg at all");
        let mut cache = Pixmaps::new();
        assert!(cache.get(&f.0, 32).is_none());
    }

    #[test]
    fn a_failure_is_cached_so_it_is_not_retried_every_keystroke() {
        let f = Scratch::new("broken.svg", b"nope");
        let mut cache = Pixmaps::new();
        assert!(cache.get(&f.0, 32).is_none());
        assert_eq!(cache.len(), 1, "the failure was not remembered");
    }

    #[test]
    fn a_zero_size_request_is_refused_rather_than_allocating_nothing() {
        let f = Scratch::new("zero.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        assert!(cache.get(&f.0, 0).is_none());
    }

    #[test]
    fn the_format_is_taken_from_the_content_not_the_extension() {
        // An SVG named .png happens; trusting the name draws nothing.
        let f = Scratch::new("liar.png", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        assert!(cache.get(&f.0, 32).is_some(), "content sniffing failed");
    }

    #[test]
    fn clearing_empties_the_cache() {
        let f = Scratch::new("clear.svg", SQUARE.as_bytes());
        let mut cache = Pixmaps::new();
        cache.get(&f.0, 32);
        cache.clear();
        assert!(cache.is_empty());
    }
}
