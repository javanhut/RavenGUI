//! The integer-scale contract.
//!
//! The rule, from the design spec §3: **a client is never handed a fractional
//! scale.** Every surface renders at a whole-number scale, and any fraction the
//! panel needs is applied exactly once, by the compositor, as the surface is
//! composed onto the panel.
//!
//! # Why
//!
//! A client told to render at 1.5× either renders at 1× and gets upscaled, or
//! renders at 2× and gets downscaled by a toolkit that does not know what the
//! panel is. Either way the glyphs pass through a resample the text rasterizer
//! did not know about, and hinted subpixel-positioned text does not survive
//! that. It is the whole reason the same terminal looks sharp on macOS and
//! mushy on Linux: macOS renders to a clean 2× backing store and downsamples
//! the *composed framebuffer* once, at the end, in one pass.
//!
//! # The consequence
//!
//! Every surface is rendered at **at least** the panel's density, never less —
//! so composing it onto the panel is always a downsample, never an upscale. A
//! downsample loses no detail the panel could have shown; an upscale invents
//! detail that was never rendered. That asymmetry is the crispness guarantee,
//! and [`OutputScale::render`] is where it is enforced.
//!
//! # How the fraction is applied
//!
//! The output carries two scales: the whole number clients are told
//! ([`OutputScale::advertised`]) and the real one the compositor lays out and
//! composes at ([`OutputScale::fractional`]). A 4K 27" is a 2560×1440 desktop
//! composed at 1.5×; its clients render at 2× and each buffer is sampled down
//! by 0.75 as it is drawn. That is one resample per surface, done by the
//! compositor, with the client none the wiser — the same as macOS's "looks
//! like 2560×1440", short of doing it to the composed frame rather than to
//! each surface.

use crate::geometry::Size;

/// Logical pixels per inch the desktop is designed against.
///
/// The traditional figure, and the one every toolkit's default font size and
/// padding was chosen for. It is the denominator that turns a panel's real DPI
/// into "how much bigger than normal should things be".
const REFERENCE_DPI: f64 = 96.0;

/// Millimetres per inch.
const MM_PER_INCH: f64 = 25.4;

/// The largest scale worth advertising.
///
/// Two. Not because three is unimaginable, but because the contract is that
/// clients only ever see whole numbers, and every panel shipping today lands
/// between 1× and 2× effective. A 3× path can be added when a panel needs it.
const MAX_ADVERTISED: u32 = 2;

/// What a single output renders at, and what it tells clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputScale {
    /// The `wl_output` scale clients are told. Always 1 or 2 — never a
    /// fraction, and never anything a client has to interpolate against.
    pub advertised: u32,
    /// The desktop's size in logical pixels: how much room there is to lay
    /// windows out in. Divided down from the panel by the *effective* scale,
    /// so a denser panel shows the same amount of desktop at a larger size.
    pub logical: Size,
    /// The scene's size at the advertised scale, in real pixels: what the
    /// surfaces add up to before the fraction is applied.
    ///
    /// Always `logical * advertised`, and always at least [`Self::physical`] —
    /// so composing onto the panel is a downsample and never an upscale.
    pub render: Size,
    /// The panel's real resolution. What actually gets scanned out.
    pub physical: Size,
}

impl OutputScale {
    /// Whether surfaces are resampled on their way to the panel.
    ///
    /// False on the common cases — a 1× desktop, and a clean 2× panel — where
    /// [`Self::fractional`] is the advertised integer and every buffer lands
    /// pixel for pixel.
    pub fn needs_resample(self) -> bool {
        self.render != self.physical
    }

    /// The scale the desktop is actually composed at: physical pixels per
    /// logical one.
    ///
    /// This is what the renderer converts positions with, and what the output
    /// reports through the protocols that can carry a fraction (`xdg_output`'s
    /// logical size). Clients are still told [`Self::advertised`]. Equal to it
    /// whenever [`Self::needs_resample`] is false.
    ///
    /// Recovered from the sizes rather than stored, so it is exactly the ratio
    /// the rest of the struct was built from and the two cannot disagree.
    pub fn fractional(self) -> f64 {
        if self.logical.w <= 0 {
            return f64::from(self.advertised);
        }
        f64::from(self.physical.w) / f64::from(self.logical.w)
    }

    /// Decide the scale for a panel of `physical` pixels and `physical_mm`.
    ///
    /// A `physical_mm` of zero on either axis means the panel did not report
    /// its size — common for virtual outputs and not rare for real ones — and
    /// there is then no way to know how dense it is. That falls back to 1×
    /// rather than guessing: guessing wrong makes every window on the desktop
    /// the wrong size, which is far worse than a desktop that is merely small
    /// on a panel that would not say how big it was.
    pub fn for_output(physical: Size, physical_mm: Size) -> Self {
        Self::from_effective(physical, effective_scale(physical, physical_mm))
    }

    /// [`Self::for_output`] with the effective scale supplied directly.
    ///
    /// Split out so the arithmetic can be tested against exact scales without
    /// going through a DPI computation that rounds.
    pub fn from_effective(physical: Size, effective: f64) -> Self {
        // Whole numbers only, and rounded up rather than to nearest: a 1.25×
        // desktop advertised as 1× would have the compositor render *below*
        // panel resolution and upscale, which is the exact failure this
        // contract exists to prevent. Rounding up costs a downsample; rounding
        // down costs sharpness, and only one of those is recoverable.
        let advertised = (effective.ceil() as u32).clamp(1, MAX_ADVERTISED);

        // The logical desktop. Divided by the *effective* scale, not the
        // advertised one — that difference is the whole point: a 1.5× panel
        // gets a 1.5×-sized desktop drawn into a 2× buffer.
        let logical = Size::new(
            divide_ceil(physical.w, effective),
            divide_ceil(physical.h, effective),
        );

        let render = Size::new(logical.w * advertised as i32, logical.h * advertised as i32);

        Self {
            advertised,
            logical,
            render,
            physical,
        }
    }
}

/// The scale a panel of this density wants, snapped down to a quarter step.
///
/// Quarter steps because the exact ratio of a panel's DPI to 96 is a number
/// like 1.5083, and letting that reach the layout means the desktop's logical
/// size changes by a pixel or two between two monitors a user would call
/// identical. Snapping makes "the same kind of screen" mean the same thing.
///
/// **Down, not to nearest.** A step is a threshold to be reached, the way
/// Windows gives 125% at 120 DPI and not before. Rounding to nearest promotes
/// every ordinary ~110 DPI monitor — a 27" 1440p, a 34" ultrawide, a 14"
/// laptop — to 1.25×, which then ceils to an advertised 2× and has the
/// compositor render a 1440p desktop into a 4096×2304 buffer and downsample it
/// every frame. Those panels are run at 1× everywhere, and should be here.
fn effective_scale(physical: Size, physical_mm: Size) -> f64 {
    if physical_mm.w <= 0 || physical_mm.h <= 0 || physical.is_empty() {
        return 1.0;
    }
    // Diagonals, so an unusual aspect ratio does not skew the result the way
    // taking one axis alone would.
    let px = f64::from(physical.w).hypot(f64::from(physical.h));
    let mm = f64::from(physical_mm.w).hypot(f64::from(physical_mm.h));
    let dpi = px / (mm / MM_PER_INCH);

    // The epsilon keeps an exact ratio from falling a step through floating
    // point: 192 DPI is exactly 2.0, and 1.9999999 would floor to 1.75.
    const EPSILON: f64 = 1e-6;
    let quarters = ((dpi / REFERENCE_DPI + EPSILON) * 4.0).floor() / 4.0;
    quarters.clamp(1.0, f64::from(MAX_ADVERTISED))
}

/// `value / divisor`, rounded up.
///
/// Up rather than down so the logical desktop never comes out a pixel short of
/// the panel: a row of physical pixels with no logical pixel over it is a line
/// at the screen edge that nothing can ever draw into.
fn divide_ceil(value: i32, divisor: f64) -> i32 {
    ((f64::from(value) / divisor).ceil() as i32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16:9 panel of `diagonal_inches`, as pixels and millimetres.
    fn panel(w: i32, h: i32, diagonal_inches: f64) -> (Size, Size) {
        let aspect = f64::from(w).hypot(f64::from(h));
        let mm = diagonal_inches * MM_PER_INCH;
        (
            Size::new(w, h),
            Size::new(
                (f64::from(w) / aspect * mm).round() as i32,
                (f64::from(h) / aspect * mm).round() as i32,
            ),
        )
    }

    #[test]
    fn a_client_is_never_told_a_fractional_scale() {
        // The contract itself. Swept across every plausible panel rather than
        // spot-checked, because one output type slipping through is one class
        // of client rendering mush.
        for (w, h, inches) in [
            (1366, 768, 14.0),
            (1920, 1080, 24.0),
            (1920, 1080, 13.3),
            (2256, 1504, 13.5),
            (2560, 1440, 27.0),
            (2880, 1800, 15.4),
            (3440, 1440, 34.0),
            (3840, 2160, 27.0),
            (3840, 2160, 43.0),
        ] {
            let (px, mm) = panel(w, h, inches);
            let scale = OutputScale::for_output(px, mm);
            assert!(
                scale.advertised == 1 || scale.advertised == 2,
                "{w}x{h} @{inches}\" advertised {}",
                scale.advertised
            );
        }
    }

    #[test]
    fn the_scene_is_never_rendered_below_panel_resolution() {
        // The crispness guarantee: the last step must be a downsample. An
        // upscale invents detail that was never rendered, and no filter
        // recovers it.
        for (w, h, inches) in [
            (1366, 768, 14.0),
            (1920, 1080, 24.0),
            (1920, 1080, 13.3),
            (2256, 1504, 13.5),
            (2560, 1440, 27.0),
            (3840, 2160, 27.0),
        ] {
            let (px, mm) = panel(w, h, inches);
            let scale = OutputScale::for_output(px, mm);
            assert!(
                scale.render.w >= px.w && scale.render.h >= px.h,
                "{w}x{h} @{inches}\" renders {:?} below panel {px:?}",
                scale.render
            );
        }
    }

    #[test]
    fn a_desktop_monitor_is_one_to_one_with_no_extra_pass() {
        // 1920x1080 at 24" is about 92 DPI: the case the reference DPI was
        // chosen for. Anything other than a straight 1x here means every
        // ordinary monitor pays for a resample it does not need.
        let (px, mm) = panel(1920, 1080, 24.0);
        let scale = OutputScale::for_output(px, mm);
        assert_eq!(scale.advertised, 1);
        assert_eq!(scale.logical, px);
        assert_eq!(scale.render, px);
        assert!(
            !scale.needs_resample(),
            "an ordinary monitor should not resample"
        );
    }

    #[test]
    fn a_clean_two_times_panel_needs_no_extra_pass_either() {
        // A 4K 27" is about 163 DPI, which snaps to 1.75 rather than 2.0 —
        // so this one DOES resample. The genuinely clean case is an exact
        // 2.0 effective scale, which is what this pins.
        let px = Size::new(3840, 2160);
        let scale = OutputScale::from_effective(px, 2.0);
        assert_eq!(scale.advertised, 2);
        assert_eq!(scale.logical, Size::new(1920, 1080));
        assert_eq!(scale.render, px);
        assert!(!scale.needs_resample());
    }

    #[test]
    fn the_specs_own_example_renders_at_two_and_downsamples() {
        // §3: "If a panel's native resolution needs something in between
        // (e.g. a 1.5x effective scale), the compositor renders the full scene
        // at 2x and resamples the composed framebuffer itself in one blit."
        let px = Size::new(2256, 1504);
        let scale = OutputScale::from_effective(px, 1.5);
        assert_eq!(scale.advertised, 2, "clients must still see a whole number");
        assert_eq!(scale.logical, Size::new(1504, 1003));
        assert_eq!(scale.render, Size::new(3008, 2006));
        assert!(scale.needs_resample());
        // And the resample is a reduction on both axes.
        assert!(scale.render.w > px.w && scale.render.h > px.h);
    }

    #[test]
    fn a_fraction_below_one_and_a_half_still_advertises_two() {
        // 1.25x rounded DOWN to 1x would render 1600x900 for a 1920x1080
        // panel and upscale it — mush, and exactly backwards.
        let px = Size::new(1920, 1080);
        let scale = OutputScale::from_effective(px, 1.25);
        assert_eq!(scale.advertised, 2);
        assert!(scale.render.w >= px.w && scale.render.h >= px.h);
        assert!(scale.needs_resample());
    }

    #[test]
    fn a_panel_that_reports_no_physical_size_falls_back_to_one() {
        // Virtual outputs and some real monitors report 0mm. Guessing dense
        // would make every window on the desktop the wrong size.
        let px = Size::new(1920, 1080);
        for mm in [Size::new(0, 0), Size::new(0, 340), Size::new(600, 0)] {
            let scale = OutputScale::for_output(px, mm);
            assert_eq!(scale.advertised, 1, "guessed a scale from {mm:?}");
            assert_eq!(scale.logical, px);
            assert!(!scale.needs_resample());
        }
    }

    #[test]
    fn an_ordinary_monitor_is_never_promoted_to_a_two_times_desktop() {
        // Regression. Snapping the DPI ratio to the NEAREST quarter turned
        // 1.13 into 1.25, which ceils to an advertised 2x — so a 27" 1440p
        // rendered a 4096x2304 buffer and downsampled it every frame. These
        // panels are run at 1x everywhere and cost nothing extra here.
        for (w, h, inches) in [
            (2560, 1440, 27.0), // 109 dpi
            (3440, 1440, 34.0), // 110 dpi
            (1366, 768, 14.0),  // 112 dpi
            (1920, 1080, 24.0), //  92 dpi
            (3840, 2160, 43.0), // 102 dpi
        ] {
            let (px, mm) = panel(w, h, inches);
            let scale = OutputScale::for_output(px, mm);
            assert_eq!(scale.advertised, 1, "{w}x{h} @{inches}\" was promoted");
            assert_eq!(scale.logical, px);
            assert!(
                !scale.needs_resample(),
                "{w}x{h} @{inches}\" resamples for nothing"
            );
        }
    }

    #[test]
    fn the_fraction_is_the_advertised_integer_unless_a_resample_is_needed() {
        // 1x and clean 2x panels compose at exactly the scale clients render
        // at, so nothing is resampled. The 4K 27" is the one that is not.
        let (px, mm) = panel(1920, 1080, 24.0);
        assert_eq!(OutputScale::for_output(px, mm).fractional(), 1.0);
        let (px, mm) = panel(2880, 1800, 15.4);
        assert_eq!(OutputScale::for_output(px, mm).fractional(), 2.0);
        let (px, mm) = panel(3840, 2160, 27.0);
        let scale = OutputScale::for_output(px, mm);
        assert_eq!(scale.fractional(), 1.5);
        assert!(scale.needs_resample());
    }

    #[test]
    fn the_fraction_maps_the_logical_desktop_back_onto_the_panel() {
        // Whatever the rounding in `logical`, the composed desktop must cover
        // the panel: logical times the fraction is the panel width exactly,
        // and the height lands within the pixel the ceil put there.
        for (w, h, inches) in [
            (1366, 768, 14.0),
            (1920, 1080, 13.3),
            (2256, 1504, 13.5),
            (3440, 1440, 34.0),
            (3840, 2160, 27.0),
        ] {
            let (px, mm) = panel(w, h, inches);
            let scale = OutputScale::for_output(px, mm);
            let f = scale.fractional();
            assert_eq!(
                (f64::from(scale.logical.w) * f).round() as i32,
                px.w,
                "{w}x{h}"
            );
            let composed_h = f64::from(scale.logical.h) * f;
            assert!(
                composed_h >= f64::from(px.h) && composed_h < f64::from(px.h) + f,
                "{w}x{h} @{inches}\": composed {composed_h} for a {}-high panel",
                px.h
            );
        }
    }

    #[test]
    fn a_four_k_twenty_seven_lands_where_people_actually_run_it() {
        // 163 dpi -> 1.5x effective -> a 2560x1440 desktop, which is what
        // every OS offers as the default for this panel.
        let (px, mm) = panel(3840, 2160, 27.0);
        let scale = OutputScale::for_output(px, mm);
        assert_eq!(scale.logical, Size::new(2560, 1440));
        assert_eq!(scale.advertised, 2);
        assert_eq!(scale.render, Size::new(5120, 2880));
        assert!(scale.needs_resample());
    }

    #[test]
    fn a_laptop_retina_panel_lands_on_two() {
        // 2880x1800 at 15.4" is about 220 DPI — unambiguously a 2x panel.
        let (px, mm) = panel(2880, 1800, 15.4);
        let scale = OutputScale::for_output(px, mm);
        assert_eq!(scale.advertised, 2);
        assert_eq!(scale.logical, Size::new(1440, 900));
        assert!(
            !scale.needs_resample(),
            "an exact 2x panel should not resample"
        );
    }

    #[test]
    fn the_logical_desktop_never_comes_up_short_of_the_panel() {
        // A row of physical pixels with no logical pixel over it is a line at
        // the screen edge nothing can draw into.
        for effective in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for (w, h) in [(1920, 1080), (2256, 1504), (1366, 768), (3440, 1440)] {
                let scale = OutputScale::from_effective(Size::new(w, h), effective);
                let covered_w = f64::from(scale.logical.w) * effective;
                let covered_h = f64::from(scale.logical.h) * effective;
                assert!(
                    covered_w >= f64::from(w) && covered_h >= f64::from(h),
                    "{w}x{h} @{effective}x leaves an uncovered edge: {:?}",
                    scale.logical
                );
            }
        }
    }

    #[test]
    fn effective_scale_snaps_to_quarters() {
        // Two monitors a user would call identical must produce an identical
        // desktop, rather than differing by a pixel because their reported
        // millimetres differ by one.
        let (px, mm) = panel(2560, 1440, 27.0);
        let snapped = effective_scale(px, mm);
        assert_eq!(
            snapped * 4.0,
            (snapped * 4.0).round(),
            "{snapped} is not a quarter step"
        );
        assert!((1.0..=2.0).contains(&snapped), "{snapped} out of range");
    }

    #[test]
    fn an_absurdly_dense_panel_is_clamped_rather_than_advertised_at_three() {
        // Clients only ever see 1 or 2. A panel denser than 2x gets a bigger
        // desktop, not a scale no client is prepared for.
        let px = Size::new(7680, 4320);
        let scale = OutputScale::from_effective(px, 4.0);
        assert_eq!(scale.advertised, MAX_ADVERTISED);
    }

    #[test]
    fn a_degenerate_output_does_not_produce_a_zero_sized_desktop() {
        // A zero-dimension logical size reaches the layout as a divide by
        // zero or a window with no area, neither of which is recoverable.
        for px in [Size::new(1, 1), Size::new(1920, 1)] {
            let scale = OutputScale::from_effective(px, 2.0);
            assert!(scale.logical.w >= 1 && scale.logical.h >= 1, "{scale:?}");
            assert!(scale.render.w >= 1 && scale.render.h >= 1, "{scale:?}");
        }
    }
    #[test]
    fn print_the_scale_table() {
        if std::env::var("SCALE_TABLE").is_err() {
            return;
        }
        println!(
            "\n{:<22} {:>5} {:>4}  {:<11} {:<11} {:<11} resample",
            "panel", "dpi", "adv", "logical", "render", "physical"
        );
        for (name, w, h, inches) in [
            ("1366x768 14\"", 1366, 768, 14.0),
            ("1920x1080 24\"", 1920, 1080, 24.0),
            ("1920x1080 13.3\"", 1920, 1080, 13.3),
            ("2256x1504 13.5\"", 2256, 1504, 13.5),
            ("2560x1440 27\"", 2560, 1440, 27.0),
            ("2880x1800 15.4\"", 2880, 1800, 15.4),
            ("3440x1440 34\"", 3440, 1440, 34.0),
            ("3840x2160 27\"", 3840, 2160, 27.0),
            ("3840x2160 43\"", 3840, 2160, 43.0),
        ] {
            let aspect = f64::from(w).hypot(f64::from(h));
            let mm = inches * 25.4;
            let phys = Size::new(w, h);
            let pmm = Size::new(
                (f64::from(w) / aspect * mm).round() as i32,
                (f64::from(h) / aspect * mm).round() as i32,
            );
            let s = OutputScale::for_output(phys, pmm);
            let dpi = aspect / inches;
            println!(
                "{name:<22} {dpi:>5.0} {:>4}  {:<11} {:<11} {:<11} {}",
                s.advertised,
                format!("{}x{}", s.logical.w, s.logical.h),
                format!("{}x{}", s.render.w, s.render.h),
                format!("{}x{}", s.physical.w, s.physical.h),
                if s.needs_resample() { "yes" } else { "no" }
            );
        }
    }
}
