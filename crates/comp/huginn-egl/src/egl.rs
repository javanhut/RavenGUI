//! The two unsafe calls, and the reasoning that discharges them.
//!
//! See the crate docs for why this is the only file in the workspace
//! permitted to write `unsafe`.

use smithay::backend::{
    egl::{EGLContext, EGLDisplay, native::EGLNativeDisplay},
    renderer::gles::GlesRenderer,
};

/// Errors from graphics initialisation.
#[derive(Debug, thiserror::Error)]
pub enum EglError {
    /// No EGL display could be opened for the given device.
    #[error("failed to initialise EGL display")]
    Display(#[source] smithay::backend::egl::Error),
    /// A display was opened but no context could be created on it.
    #[error("failed to create EGL context")]
    Context(#[source] smithay::backend::egl::Error),
    /// The context was created but the GLES renderer rejected it.
    #[error("failed to create GLES renderer")]
    Renderer(#[source] smithay::backend::renderer::gles::GlesError),
}

/// Build a GLES renderer for a native display, typically a GBM device.
///
/// This is the single entry point to graphics initialisation for the udev
/// backend. It is safe to call: both of smithay's underlying obligations are
/// discharged below.
///
/// # Threading
///
/// The returned renderer is bound to the calling thread and must be used only
/// from it. That is not merely a convention — it is half of what makes the
/// `GlesRenderer::new` call below sound. Huginn's event loop is single
/// threaded, so this holds by construction.
pub fn renderer_for<N>(native: N) -> Result<GlesRenderer, EglError>
where
    N: EGLNativeDisplay + 'static,
{
    // SAFETY: smithay caches EGLDisplay instances internally, because
    // eglGetPlatformDisplay returns the same underlying display for the same
    // arguments. The obligation is that no code outside smithay creates or
    // terminates an EGL display for this device, since an external
    // eglTerminate would invalidate the instance smithay still believes is
    // live.
    //
    // Huginn links no other EGL user: `unsafe_code = "forbid"` across every
    // other crate in the workspace means no sibling crate can call into EGL at
    // all, and this function is the only caller of EGLDisplay::new. `native` is
    // moved in, so the caller cannot retain it to open a second display for the
    // same device behind our back.
    let display = unsafe { EGLDisplay::new(native) }.map_err(EglError::Display)?;

    // Safe: EGLContext::new carries no contract beyond a valid display.
    let context = EGLContext::new(&display).map_err(EglError::Context)?;

    // SAFETY: GlesRenderer::new is undefined behaviour if the given EGLContext
    // is current on another thread. `context` was created immediately above and
    // moved in here, so no other value refers to it and it has never been made
    // current anywhere. Huginn's compositor is single threaded — the renderer
    // never crosses a thread boundary after this point — so no other thread can
    // subsequently make it current either.
    let renderer = unsafe { GlesRenderer::new(context) }.map_err(EglError::Renderer)?;

    Ok(renderer)
}
