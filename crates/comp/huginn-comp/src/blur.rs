//! Blurring the desktop behind a panel.
//!
//! §4: the launcher fades and scales in, and the desktop behind it blurs, with
//! the radius animated over the same 150ms. The blur is what separates the
//! panel from what is behind it without a drop shadow doing the work — and it
//! is the reason the launcher reads as being *in front of* the desktop rather
//! than pasted onto it.
//!
//! # How
//!
//! The scene's windows are rendered into an offscreen texture, that texture is
//! drawn through a Gaussian shader, cropped to the panel's rectangle (see
//! `render::blur_element`) over the sharp desktop, and the panel goes on top.
//! The whole output is blurred even though only the patch under the panel is
//! shown, because the kernel needs the pixels past the panel's edge to soften
//! the ones just inside it. Two
//! passes — horizontal then vertical — because a separable Gaussian costs
//! `2n` samples per pixel where the naive square costs `n²`: at a 24-pixel
//! radius that is 98 samples against 2401, which is the difference between a
//! blur that fits in a frame and one that does not.
//!
//! The scene pass draws the elements with the output's scale, the same one the
//! backend would have drawn them with — an element's size comes from the scale
//! it is drawn at, so anything else draws the desktop the wrong size into the
//! texture. The two blur passes are texture-to-texture and work in physical
//! pixels at 1:1; only the last element, the one handed back to the backend,
//! is sized in logical pixels so that the backend's scale maps it back onto
//! the panel exactly.
//!
//! # Never verified on a screen
//!
//! This compiles and its kernel is tested, but no part of the GPU path has
//! been run: shader compilation errors surface only at runtime, on real
//! hardware. The fallbacks below are what make that acceptable to ship
//! untested — the worst case they allow is a panel that does not blur.
//!
//! # If it fails
//!
//! Every part of this is allowed to fail into "no blur". A shader that will not
//! compile on someone's GPU, or an offscreen buffer that cannot be allocated,
//! must cost the desktop a visual effect and not the ability to draw. See
//! [`Blur::compile`], which returns `None` rather than an error worth
//! propagating.

/// The largest blur radius, in pixels at 1×.
///
/// The shader samples a fixed number of taps regardless, so this only sets how
/// far apart they are; past about this the taps separate enough that the blur
/// starts to band instead of smoothing.
pub(crate) const MAX_RADIUS: f32 = 18.0;

/// Taps per pass, each side of centre.
///
/// Nine, so a pass samples 19 texels and the pair costs 38. Enough that the
/// Gaussian is not visibly truncated at [`MAX_RADIUS`], few enough to stay
/// within the uniform array a GLES 2 shader can rely on.
pub(crate) const TAPS: usize = 9;

/// Gaussian weights for [`TAPS`] offsets each side of centre, normalized.
///
/// Normalized because the weights are what the shader multiplies each sample
/// by before summing: if they do not add to one the image comes back brighter
/// or darker than it went in, and a blur that dims the desktop as it applies
/// looks like a fade rather than a blur.
///
/// `sigma` is the standard deviation in taps. A third of the radius is the
/// usual choice — it puts three standard deviations at the edge of the kernel,
/// by which point the Gaussian has fallen to under half a percent and the
/// truncation is invisible.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn weights(sigma: f32) -> [f32; TAPS + 1] {
    let sigma = sigma.max(0.0001);
    let mut weights = [0.0_f32; TAPS + 1];
    // Only one side is computed; the kernel is symmetric, so the shader reads
    // both directions from the same table.
    for (offset, weight) in weights.iter_mut().enumerate() {
        let x = offset as f32;
        *weight = (-(x * x) / (2.0 * sigma * sigma)).exp();
    }
    // The centre tap is counted once, the rest twice — once each side.
    let total: f32 = weights[0] + weights[1..].iter().sum::<f32>() * 2.0;
    for weight in &mut weights {
        *weight /= total;
    }
    weights
}

/// The blur radius for a panel that is `reveal` of the way open.
///
/// Animated with the reveal so the desktop softens as the panel arrives, which
/// is what §4 asks for — a blur that snapped to full strength would read as a
/// separate event from the panel appearing.
pub(crate) fn radius_for(reveal: f32) -> f32 {
    MAX_RADIUS * reveal.clamp(0.0, 1.0)
}

/// The fragment shader, one separable pass.
///
/// `//_DEFINES` is required by smithay and replaced with its own `#define`s.
/// The uniforms it adds beyond the ones every texture shader gets:
///
/// - `blur_dir` — the step between taps, in texture coordinates. Horizontal on
///   one pass and vertical on the other; the same shader runs both, which is
///   the whole point of the kernel being separable.
/// - `blur_sigma` — the Gaussian's standard deviation, in taps.
///
/// The weights are computed **in the shader** rather than passed in, because
/// smithay's uniform types have no array-of-floats and ten scalar uniforms
/// would be worse than ten `exp()` calls. [`weights`] is the same formula in
/// Rust: it is the executable specification this mirrors, and what the tests
/// pin. Normalizing by the running total also makes "the weights sum to one"
/// true by construction here rather than by arithmetic that could drift.
pub(crate) const SHADER: &str = r#"
precision mediump float;
//_DEFINES

uniform sampler2D tex;
uniform float alpha;
varying vec2 v_coords;

uniform vec2 blur_dir;
uniform float blur_sigma;

void main() {
    float sigma = max(blur_sigma, 0.0001);
    float denom = 2.0 * sigma * sigma;

    // The centre tap, whose weight is exp(0) = 1.
    vec4 sum = texture2D(tex, v_coords);
    float total = 1.0;

    for (int i = 1; i < 10; i++) {
        float x = float(i);
        float w = exp(-(x * x) / denom);
        vec2 step = blur_dir * x;
        sum += (texture2D(tex, v_coords + step) + texture2D(tex, v_coords - step)) * w;
        total += 2.0 * w;
    }
    gl_FragColor = (sum / total) * alpha;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_weights_sum_to_one() {
        // The property that matters: a kernel that does not sum to one changes
        // the image's brightness, so applying the blur would dim or brighten
        // the desktop as it came in. Counted the way the shader does — centre
        // once, every other tap twice.
        for sigma in [0.5, 1.0, 2.0, 3.0, 6.0] {
            let w = weights(sigma);
            let total = w[0] + w[1..].iter().sum::<f32>() * 2.0;
            assert!(
                (total - 1.0).abs() < 1e-4,
                "sigma {sigma} sums to {total}, not 1"
            );
        }
    }

    #[test]
    fn the_kernel_falls_away_from_the_centre() {
        // A Gaussian that is not monotonically decreasing is not a Gaussian,
        // and the artefact it produces looks like ringing rather than blur.
        let w = weights(3.0);
        for pair in w.windows(2) {
            assert!(pair[0] >= pair[1], "weights rise: {w:?}");
        }
    }

    #[test]
    fn a_tiny_sigma_keeps_almost_everything_at_the_centre() {
        // Which is what makes radius 0 mean "no blur" rather than "average of
        // everything nearby" — the animation starts there on every open.
        let w = weights(0.01);
        assert!(w[0] > 0.99, "centre weight was only {}", w[0]);
    }

    #[test]
    fn a_zero_sigma_does_not_divide_by_zero() {
        // radius_for(0.0) is the first frame of every reveal, so this is the
        // common case rather than an edge one.
        let w = weights(0.0);
        assert!(w.iter().all(|v| v.is_finite()), "produced {w:?}");
        assert!(w[0] > 0.99);
    }

    #[test]
    fn a_wide_sigma_spreads_without_truncating_visibly() {
        // Three standard deviations at the edge of the kernel: the outermost
        // tap should carry very little, or the Gaussian is being cut off where
        // it still had weight and the blur shows a hard edge.
        let w = weights(TAPS as f32 / 3.0);
        assert!(
            w[TAPS] < w[0] / 50.0,
            "outermost tap is {} of centre",
            w[TAPS] / w[0]
        );
    }

    #[test]
    fn the_radius_follows_the_reveal() {
        assert_eq!(radius_for(0.0), 0.0);
        assert_eq!(radius_for(1.0), MAX_RADIUS);
        assert!(radius_for(0.5) > 0.0 && radius_for(0.5) < MAX_RADIUS);
        // And is clamped, since a spring overshoots past 1.
        assert_eq!(radius_for(1.4), MAX_RADIUS);
        assert_eq!(radius_for(-0.2), 0.0);
    }

    #[test]
    fn the_shader_declares_what_smithay_needs_to_substitute() {
        // Without this marker smithay cannot inject its own defines and the
        // shader fails to compile at runtime — where the only symptom is the
        // blur silently never appearing.
        assert!(
            SHADER.contains("//_DEFINES"),
            "the defines marker is missing"
        );
        assert!(SHADER.contains("v_coords"), "the vertex varying is missing");
        assert!(
            SHADER.contains("uniform sampler2D tex"),
            "the sampler is missing"
        );
        assert!(
            SHADER.contains("uniform float alpha"),
            "the alpha uniform is missing"
        );
    }

    #[test]
    fn the_shader_loop_matches_the_tap_count() {
        // The GLES 2 shader cannot index a uniform array by a runtime value,
        // so the loop bound is written out — and it has to agree with TAPS or
        // the weights past the bound are computed and never read.
        assert_eq!(TAPS, 9);
        assert!(
            SHADER.contains("i < 10"),
            "the shader's loop does not match TAPS = {TAPS}"
        );
    }
}

// ---------------------------------------------------------------------------
// The GPU pass
// ---------------------------------------------------------------------------

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::element::TextureShaderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform};
use smithay::backend::renderer::{Bind, Color32F, Frame, Offscreen, Renderer};
use smithay::utils::{Logical, Physical, Rectangle, Scale, Size, Transform};

use crate::render::HuginnElement;

/// The compiled blur, and the textures it works in.
///
/// Held across frames: compiling a shader and allocating two full-screen
/// textures per frame would cost more than the blur itself.
#[derive(Debug)]
pub(crate) struct Blur {
    program: GlesTexProgram,
    /// The scene, and the horizontal pass. Reallocated only when the output
    /// resizes.
    scene: Option<(GlesTexture, GlesTexture, Size<i32, Physical>)>,
}

impl Blur {
    /// Compile the shader, or give up.
    ///
    /// `None` on failure rather than an error to propagate: a GPU that will not
    /// compile this must cost the desktop a visual effect, not the ability to
    /// draw. Everything downstream treats `None` as "no blur" and takes the
    /// ordinary path.
    pub(crate) fn compile(renderer: &mut GlesRenderer) -> Option<Self> {
        match renderer.compile_custom_texture_shader(
            SHADER,
            &[
                smithay::backend::renderer::gles::UniformName::new(
                    "blur_dir",
                    smithay::backend::renderer::gles::UniformType::_2f,
                ),
                smithay::backend::renderer::gles::UniformName::new(
                    "blur_sigma",
                    smithay::backend::renderer::gles::UniformType::_1f,
                ),
            ],
        ) {
            Ok(program) => {
                tracing::info!("blur shader compiled");
                Some(Self {
                    program,
                    scene: None,
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "blur shader did not compile; panels will not blur");
                None
            }
        }
    }

    /// Render `elements` into a texture, blur it, and return it as an element.
    ///
    /// `size` is the output in physical pixels and `scale` is what the caller
    /// will draw the returned element — and would have drawn `elements` — with.
    ///
    /// `None` if anything fails, in which case the caller draws `elements`
    /// itself exactly as it would have without a blur.
    pub(crate) fn pass(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[HuginnElement],
        size: Size<i32, Physical>,
        scale: f64,
        radius: f32,
    ) -> Option<TextureShaderElement> {
        if radius <= 0.05 || size.w <= 0 || size.h <= 0 || scale <= 0.0 {
            return None;
        }
        self.ensure_textures(renderer, size)?;
        let context = Renderer::context_id(renderer);
        // A third of the radius puts three standard deviations at the edge of
        // the kernel, where the Gaussian has fallen below half a percent.
        let sigma = radius / 3.0;
        let full = Rectangle::from_size(size);

        // Pass 0: the scene, unblurred, into `scene`.
        {
            let (scene, _, _) = self.scene.as_mut()?;
            let mut fb = renderer.bind(scene).ok()?;
            let mut frame = renderer.render(&mut fb, size, Transform::Normal).ok()?;
            frame
                .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &[full])
                .ok()?;
            let _ = smithay::backend::renderer::utils::draw_render_elements::<GlesRenderer, _, _>(
                &mut frame,
                Scale::from(scale),
                elements,
                &[full],
            );
            // The sync point is dropped: the next pass samples this texture
            // through the same context, which orders them for us.
            let _sync = frame.finish().ok()?;
        }

        // Pass 1: horizontal, `scene` into `horizontal`.
        {
            let (scene, horizontal, _) = self.scene.as_mut()?;
            let source = scene.clone();
            let mut fb = renderer.bind(horizontal).ok()?;
            let mut frame = renderer.render(&mut fb, size, Transform::Normal).ok()?;
            frame
                .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &[full])
                .ok()?;
            let element = shader_element(
                context.clone(),
                &source,
                &self.program,
                // Drawn at 1:1 below, so physical pixels are the logical ones.
                Size::from((size.w, size.h)),
                [1.0 / size.w as f32, 0.0],
                sigma,
            );
            let _ = smithay::backend::renderer::utils::draw_render_elements::<GlesRenderer, _, _>(
                &mut frame,
                1.0,
                std::slice::from_ref(&element),
                &[full],
            );
            let _sync = frame.finish().ok()?;
        }

        // Pass 2 is the caller's: the vertical pass is this element, drawn into
        // whatever framebuffer the frame is going to — at the caller's scale,
        // so it is sized in logical pixels to come out as the whole panel.
        let logical: Size<i32, Logical> = size.to_f64().to_logical(scale).to_i32_round();
        let (_, horizontal, _) = self.scene.as_ref()?;
        Some(shader_element(
            context,
            horizontal,
            &self.program,
            logical,
            [0.0, 1.0 / size.h as f32],
            sigma,
        ))
    }

    /// Allocate the working textures, reusing them across frames.
    fn ensure_textures(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
    ) -> Option<()> {
        if self
            .scene
            .as_ref()
            .is_some_and(|(_, _, held)| *held == size)
        {
            return Some(());
        }
        let buffer_size = Size::from((size.w, size.h));
        let scene = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
        let horizontal = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
        self.scene = Some((scene, horizontal, size));
        Some(())
    }
}

/// One blur pass as a drawable element, `size` being what it will cover in
/// the coordinates of whoever draws it.
fn shader_element(
    context: smithay::backend::renderer::ContextId<GlesTexture>,
    texture: &GlesTexture,
    program: &GlesTexProgram,
    size: Size<i32, Logical>,
    direction: [f32; 2],
    sigma: f32,
) -> TextureShaderElement {
    let inner = TextureRenderElement::from_static_texture(
        Id::new(),
        context,
        (0.0, 0.0),
        texture.clone(),
        1,
        Transform::Normal,
        Some(1.0),
        None,
        Some((size.w, size.h).into()),
        None,
        Kind::Unspecified,
    );
    TextureShaderElement::new(
        inner,
        program.clone(),
        vec![
            Uniform::new("blur_dir", direction),
            Uniform::new("blur_sigma", sigma),
        ],
    )
}
