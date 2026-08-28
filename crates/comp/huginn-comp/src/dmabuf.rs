//! `zwp_linux_dmabuf_v1`: letting clients hand us GPU buffers directly.
//!
//! Without this global a client has no way to negotiate hardware buffers, so it
//! falls back to `wl_shm` and every frame is copied through the CPU. The
//! renderer could already import dmabufs — `GlesRenderer` implements
//! `ImportDmaWl`, which is what satisfies `ImportAll` on the non-`use_system_lib`
//! path — clients simply had no way to find that out.
//!
//! Note this is the *modern* path. The legacy alternative is `wl_drm` via
//! `ImportEgl::bind_wl_display`, which is gated behind smithay's
//! `use_system_lib` and would swap the pure-Rust Wayland backend for system
//! libwayland. dmabuf costs no such dependency and is what the udev backend
//! needs regardless.
//!
//! # Deferred validation
//!
//! A dmabuf must be validated by actually importing it, which needs the
//! renderer with its EGL context current. The renderer lives in the backend's
//! render loop, not in [`Huginn`], so imports are queued here and drained by
//! the backend once per frame. [`ImportNotifier`] exists for exactly this: it
//! owns the buffer and can be completed later.

use smithay::{
    backend::allocator::{Format, dmabuf::Dmabuf},
    backend::renderer::ImportDma,
    delegate_dmabuf,
    reexports::wayland_server::DisplayHandle,
    wayland::dmabuf::{
        DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
    },
};

use crate::state::Huginn;

/// Dmabuf imports awaiting validation against the renderer.
#[derive(Debug, Default)]
pub(crate) struct PendingImports(Vec<(Dmabuf, ImportNotifier)>);

impl PendingImports {
    fn push(&mut self, dmabuf: Dmabuf, notifier: ImportNotifier) {
        self.0.push((dmabuf, notifier));
    }
}

impl Huginn {
    /// Advertise `zwp_linux_dmabuf_v1` for `main_device` with `formats`.
    ///
    /// `main_device` is the DRM node the compositor renders on. Clients need it
    /// to allocate buffers the compositor can actually import — on a multi-GPU
    /// machine, guessing wrong means an import failure per frame and a silent
    /// fall back to software.
    pub(crate) fn enable_dmabuf(
        &mut self,
        dh: &DisplayHandle,
        main_device: libc::dev_t,
        formats: impl IntoIterator<Item = Format>,
    ) {
        let formats: Vec<Format> = formats.into_iter().collect();
        let count = formats.len();

        let feedback = match DmabufFeedbackBuilder::new(main_device, formats).build() {
            Ok(feedback) => feedback,
            Err(e) => {
                tracing::warn!(error = %e, "dmabuf feedback rejected; clients stay on shm");
                return;
            }
        };

        let global = self
            .dmabuf_state
            .create_global_with_default_feedback::<Self>(dh, &feedback);
        self.dmabuf_global = Some(global);
        tracing::info!(
            formats = count,
            "dmabuf enabled; clients can use hardware buffers"
        );
    }

    /// Take the queued imports so the backend can validate them.
    pub(crate) fn take_pending_dmabufs(&mut self) -> Vec<(Dmabuf, ImportNotifier)> {
        std::mem::take(&mut self.pending_dmabufs.0)
    }
}

/// Answer every queued dmabuf import, importing it to find out.
///
/// Every backend must call this once per frame, with the renderer's context
/// current. A notifier that is never answered leaves the client waiting for a
/// buffer that is never acknowledged: the window maps, takes its configure, and
/// then never paints. Only `wl_shm` clients — the panel — stay visible, which
/// makes it look like a client bug rather than a missing call.
pub(crate) fn import_pending<R: ImportDma>(renderer: &mut R, state: &mut Huginn) {
    for (dmabuf, notifier) in state.take_pending_dmabufs() {
        match renderer.import_dmabuf(&dmabuf, None) {
            Ok(_texture) => {
                let _ = notifier.successful::<Huginn>();
            }
            Err(e) => {
                tracing::debug!(error = %e, "rejected a client dmabuf");
                notifier.failed();
            }
        }
    }
}

impl DmabufHandler for Huginn {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Queue rather than answer now: validating means importing, and the
        // renderer is not reachable from here. The backend drains this next
        // frame and answers the notifier either way.
        self.pending_dmabufs.push(dmabuf, notifier);
        // Only the render loop can answer the notifier, so make sure one runs.
        self.queue_redraw();
    }
}

delegate_dmabuf!(Huginn);
