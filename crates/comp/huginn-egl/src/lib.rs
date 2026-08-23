//! Safe wrappers around smithay's EGL/GLES bring-up.
//!
//! # Why this crate exists
//!
//! Every other crate in this workspace inherits `unsafe_code = "forbid"` from
//! `[workspace.lints]`, which cannot be overridden by an `#[allow]` anywhere
//! inside the crate. But a real compositor cannot avoid `unsafe` entirely:
//! smithay exposes 28 `pub unsafe fn`, of which a DRM/GLES compositor must call
//! roughly six.
//!
//! | Function | Contract the caller must uphold |
//! |---|---|
//! | `EGLDisplay::new` | the native display outlives the `EGLDisplay` |
//! | `EGLContext::new` / `make_current` | not current on any other thread |
//! | `EGLSurface::new` | the native window outlives the surface |
//! | `GlesRenderer::new` | called with the passed context current, and used only from that thread |
//!
//! Rather than sprinkle those obligations across the compositor, they are
//! discharged once, here, behind types whose ownership makes the contract
//! structurally true — and this is the only place a reviewer has to audit.
//!
//! # Rules for this crate
//!
//! 1. It stays small. If it grows past a few hundred lines, something that
//!    belongs in `huginn-comp` has leaked in.
//! 2. Every public function is safe to call. `unsafe` never appears in this
//!    crate's public signatures; if a contract cannot be discharged internally,
//!    the wrapper is wrong.
//! 3. Every `unsafe` block carries a `// SAFETY:` comment naming the specific
//!    invariant and how the surrounding type guarantees it.
//!
//! # Status
//!
//! Unimplemented. The wrappers are written against the real backend on the
//! Linux host, where they can actually be compiled and run — writing EGL
//! initialisation that has never been through a compiler is how you get
//! plausible code that deadlocks on first light.

/// Errors from graphics initialisation.
#[derive(Debug, thiserror::Error)]
pub enum EglError {
    /// No EGL display could be opened for the given device.
    #[error("failed to initialise EGL display")]
    Display,
}
