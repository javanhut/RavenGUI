//! Safe wrappers around smithay's EGL/GLES bring-up.
//!
//! # Why this crate exists
//!
//! Every other crate in this workspace inherits `unsafe_code = "forbid"` from
//! `[workspace.lints]`, which cannot be overridden by an `#[allow]` anywhere
//! inside the crate. But a compositor driving DRM/KMS directly cannot avoid
//! `unsafe` entirely: creating an EGL display for a GBM device and building a
//! GLES renderer on it are both `unsafe fn` in smithay.
//!
//! Exactly two calls need it, and both are here:
//!
//! | Call | Obligation |
//! |---|---|
//! | [`EGLDisplay::new`] | nothing outside smithay may create or terminate EGL displays for this device |
//! | [`GlesRenderer::new`] | the context must not be current on another thread |
//!
//! Rather than spread those obligations across the compositor, they are
//! discharged once, here, and this is the only file a reviewer has to audit for
//! memory safety.
//!
//! Note the nested `winit` backend needs none of this — `winit::init` is a safe
//! function that builds its renderer internally. This crate exists solely for
//! the udev/DRM path.
//!
//! # Rules for this crate
//!
//! 1. It stays small. If it grows past a few hundred lines, something that
//!    belongs in `huginn-comp` has leaked in.
//! 2. Every public function is safe to call. `unsafe` never appears in this
//!    crate's public signatures; if a contract cannot be discharged internally,
//!    the wrapper is wrong.
//! 3. Every `unsafe` block carries a `// SAFETY:` comment naming the specific
//!    invariant and how the surrounding code guarantees it.
//!    `clippy::undocumented_unsafe_blocks` makes that a build error, not a
//!    habit.

// The wrappers exist only for the udev/DRM path, which is Linux-only.
// Off Linux this crate is deliberately empty rather than absent, so that
// `cargo check --workspace` still covers the manifest and its lint opt-out.
#[cfg(target_os = "linux")]
mod egl;

#[cfg(target_os = "linux")]
pub use egl::{EglError, renderer_for};
