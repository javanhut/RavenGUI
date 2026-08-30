//! DRM devices other than the one the compositor renders on.
//!
//! A laptop with a discrete GPU wires some of its ports to that GPU, and a
//! DisplayLink dock shows up as a DRM device of its own with no GPU behind it
//! at all. Either way the connector lives on a device the compositor does not
//! render on, and lighting it up means getting frames from the primary GPU
//! onto that device's scan-out buffers.
//!
//! Two kinds, by what the device can do:
//!
//! - [`Scanout::Gpu`]: the device has a render node and EGL comes up on it. The
//!   primary renders the screen's view into a linear dmabuf it allocated; the
//!   secondary imports that buffer as a texture and draws it onto its own
//!   scan-out surface through a [`DrmOutputManager`] of its own. One copy, on
//!   the GPU, per frame.
//! - [`Scanout::Dumb`]: no render node, or EGL refused it -- `udl`, `evdi`,
//!   and any card whose driver has no Mesa backend. The primary renders into an
//!   offscreen texture, reads it back, and the pixels are written into a dumb
//!   buffer the device page-flips. One copy over the CPU per frame, which is
//!   what a DisplayLink adapter costs on every operating system.
//!
//! smithay's own multi-GPU renderer covers only the first case (it requires a
//! GL renderer on every device), and its `DrmOutputManager` cannot drive dumb
//! buffers (its allocator must be `Clone`, and `DumbAllocator` is not). Hence
//! this module: the first case is done here for symmetry with the second, so
//! the primary's render path stays exactly what it is and every secondary
//! screen is "render the view, then hand it over".

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{
            Fourcc, Modifier,
            dmabuf::{AsDmabuf, Dmabuf},
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmNode, DrmSurface, NodeType,
            exporter::gbm::GbmFramebufferExporter, output::DrmOutputManager,
        },
        renderer::{ImportDma, gles::GlesRenderer, gles::GlesTexture},
        session::{Session, libseat::LibSeatSession},
    },
    reexports::{
        calloop::RegistrationToken,
        drm::buffer::Buffer as _,
        drm::control::{
            Device as ControlDevice, Mode as DrmMode, connector, crtc, dumbbuffer::DumbBuffer,
            framebuffer,
        },
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Physical, Rectangle, Size, Transform},
};

pub(super) type Manager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// The pixel format every bridge uses. Alpha-capable so the primary can
/// render into it exactly as it renders a scan-out buffer, and the format GL
/// mandates for read-back.
pub(super) const BRIDGE_FORMAT: Fourcc = Fourcc::Argb8888;

/// A DRM device the compositor drives but does not render on.
pub(super) struct Secondary {
    pub(super) path: PathBuf,
    pub(super) node: DrmNode,
    pub(super) scanout: Scanout,
    /// The event-loop registration of the device's vblank notifier, so it can
    /// be withdrawn when the device is unplugged.
    pub(super) token: Option<RegistrationToken>,
}

/// How a secondary device gets its pixels. See the module docs.
pub(super) enum Scanout {
    Gpu {
        manager: Box<Manager>,
        renderer: Box<GlesRenderer>,
    },
    Dumb {
        device: DrmDevice,
        fd: DrmDeviceFd,
    },
}

impl std::fmt::Debug for Secondary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secondary")
            .field("path", &self.path)
            .field(
                "kind",
                &match self.scanout {
                    Scanout::Gpu { .. } => "gpu",
                    Scanout::Dumb { .. } => "dumb",
                },
            )
            .finish()
    }
}

impl Secondary {
    /// Open a device through the session and decide how to drive it.
    ///
    /// A device with a render node is tried as a GPU first; if EGL or GBM will
    /// not come up on it -- a card without a Mesa driver, or one whose driver
    /// exposes a render node it cannot back -- it is driven with dumb buffers
    /// instead, which every KMS driver supports. Returns the device and its
    /// vblank notifier, which the caller registers.
    pub(super) fn open(
        session: &mut LibSeatSession,
        path: &Path,
    ) -> Result<(Self, DrmDeviceNotifier)> {
        let node = DrmNode::from_path(path).with_context(|| format!("{path:?} as a DRM node"))?;
        let fd = session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|e| anyhow::anyhow!("session refused to open {path:?}: {e}"))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (device, notifier) =
            DrmDevice::new(fd.clone(), true).with_context(|| format!("DRM device at {path:?}"))?;

        let render_node = node.node_with_type(NodeType::Render).and_then(Result::ok);
        let as_gpu = render_node.and_then(|render| {
            let gbm = GbmDevice::new(fd.clone()).ok()?;
            let renderer = match huginn_egl::renderer_for(gbm.clone()) {
                Ok(renderer) => renderer,
                Err(e) => {
                    tracing::info!(?path, error = %e, "no GL on this device; driving it with dumb buffers");
                    return None;
                }
            };
            let formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
            Some((gbm, renderer, formats, render))
        });

        let scanout = match as_gpu {
            Some((gbm, renderer, formats, render)) => {
                let allocator = GbmAllocator::new(
                    gbm.clone(),
                    GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
                );
                let manager = DrmOutputManager::new(
                    device,
                    allocator,
                    // Only buffers already on this GPU are candidates for
                    // direct scan-out; anything else is the bridge's job.
                    GbmFramebufferExporter::new(gbm.clone(), Some(render)),
                    Some(gbm),
                    [Fourcc::Xrgb8888, Fourcc::Argb8888],
                    formats,
                );
                tracing::info!(?path, ?render, "secondary GPU up");
                Scanout::Gpu {
                    manager: Box::new(manager),
                    renderer: Box::new(renderer),
                }
            }
            None => {
                tracing::info!(
                    ?path,
                    "display-only device up; frames are copied over the CPU"
                );
                Scanout::Dumb { device, fd }
            }
        };

        Ok((
            Self {
                path: path.to_owned(),
                node,
                scanout,
                token: None,
            },
            notifier,
        ))
    }

    pub(super) fn device(&self) -> &DrmDevice {
        match &self.scanout {
            Scanout::Gpu { manager, .. } => manager.device(),
            Scanout::Dumb { device, .. } => device,
        }
    }

    pub(super) fn pause(&mut self) {
        match &mut self.scanout {
            Scanout::Gpu { manager, .. } => manager.pause(),
            Scanout::Dumb { device, .. } => device.pause(),
        }
    }

    pub(super) fn activate(&mut self, disable_connectors: bool) -> Result<()> {
        match &mut self.scanout {
            Scanout::Gpu { manager, .. } => manager.activate(disable_connectors)?,
            Scanout::Dumb { device, .. } => device.activate(disable_connectors)?,
        }
        Ok(())
    }
}

/// The buffer a GPU-kind screen is rendered into on the primary and drawn
/// from on the secondary.
///
/// Linear, because that is the one layout every GPU agrees on: a tiled
/// buffer from one vendor is noise to another. The texture is the
/// secondary's import of the same memory, made once; each frame the primary
/// writes into the dmabuf and the secondary samples it.
#[derive(Debug)]
pub(super) struct Bridge {
    pub(super) dmabuf: Dmabuf,
    pub(super) texture: GlesTexture,
    pub(super) size: Size<i32, Physical>,
}

impl Bridge {
    pub(super) fn new(
        primary: &mut GbmAllocator<DrmDeviceFd>,
        secondary: &mut GlesRenderer,
        size: Size<i32, Physical>,
    ) -> Result<Self> {
        let buffer = primary
            .create_buffer_with_flags(
                size.w as u32,
                size.h as u32,
                BRIDGE_FORMAT,
                &[Modifier::Linear],
                GbmBufferFlags::RENDERING | GbmBufferFlags::LINEAR,
            )
            .context("allocating the bridge buffer on the primary GPU")?;
        let dmabuf = buffer.export().context("exporting the bridge buffer")?;
        let texture = secondary
            .import_dmabuf(&dmabuf, None)
            .context("importing the bridge buffer on the secondary GPU")?;
        Ok(Self {
            dmabuf,
            texture,
            size,
        })
    }
}

/// One framebuffer on a display-only device.
struct DumbFb {
    buffer: DumbBuffer,
    fb: framebuffer::Handle,
}

/// A CRTC on a display-only device, double-buffered with dumb buffers.
pub(super) struct DumbSurface {
    pub(super) surface: DrmSurface,
    fd: DrmDeviceFd,
    buffers: [DumbFb; 2],
    /// The buffer on screen; the other is drawn into next.
    front: usize,
    /// The first commit is a modeset; page flips follow.
    modeset_done: bool,
    size: Size<i32, Physical>,
    /// The primary's offscreen texture for this screen, kept across frames.
    pub(super) texture: Option<GlesTexture>,
}

impl std::fmt::Debug for DumbSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DumbSurface")
            .field("crtc", &self.surface.crtc())
            .field("size", &self.size)
            .finish()
    }
}

impl DumbSurface {
    pub(super) fn new(
        device: &mut DrmDevice,
        fd: &DrmDeviceFd,
        crtc: crtc::Handle,
        mode: DrmMode,
        connectors: &[connector::Handle],
    ) -> Result<Self> {
        let surface = device
            .create_surface(crtc, mode, connectors)
            .context("creating the scan-out surface")?;
        let (w, h) = mode.size();
        let size = Size::from((i32::from(w), i32::from(h)));
        let buffers = [Self::framebuffer(fd, size)?, Self::framebuffer(fd, size)?];
        Ok(Self {
            surface,
            fd: fd.clone(),
            buffers,
            front: 0,
            modeset_done: false,
            size,
            texture: None,
        })
    }

    fn framebuffer(fd: &DrmDeviceFd, size: Size<i32, Physical>) -> Result<DumbFb> {
        let buffer = fd
            .create_dumb_buffer((size.w as u32, size.h as u32), Fourcc::Xrgb8888, 32)
            .context("creating a dumb buffer")?;
        let fb = fd
            .add_framebuffer(&buffer, 24, 32)
            .context("adding a framebuffer for the dumb buffer")?;
        Ok(DumbFb { buffer, fb })
    }

    pub(super) const fn size(&self) -> Size<i32, Physical> {
        self.size
    }

    /// Switch mode: new buffers at the new size, and a modeset on the next
    /// frame.
    pub(super) fn use_mode(&mut self, mode: DrmMode) -> Result<()> {
        self.surface.use_mode(mode).context("staging the mode")?;
        let (w, h) = mode.size();
        let size = Size::from((i32::from(w), i32::from(h)));
        let fresh = [
            Self::framebuffer(&self.fd, size)?,
            Self::framebuffer(&self.fd, size)?,
        ];
        let old = std::mem::replace(&mut self.buffers, fresh);
        for fb in old {
            self.release(fb);
        }
        self.size = size;
        self.modeset_done = false;
        self.texture = None;
        Ok(())
    }

    /// Put `pixels` -- tightly packed 32-bit rows of the surface's size -- on
    /// screen. Copies into the back buffer and flips to it; the vblank comes
    /// through the device's notifier like any other.
    pub(super) fn present(&mut self, pixels: &[u8]) -> Result<()> {
        let back = 1 - self.front;
        let width_bytes = self.size.w as usize * 4;
        {
            let buffer = &mut self.buffers[back].buffer;
            let pitch = buffer.pitch() as usize;
            let mut mapping = self
                .fd
                .map_dumb_buffer(buffer)
                .context("mapping the dumb buffer")?;
            let dst = mapping.as_mut();
            // Row by row: the kernel's pitch is not necessarily the tight one.
            for (row, src) in pixels.chunks_exact(width_bytes).enumerate() {
                let at = row * pitch;
                if at + width_bytes > dst.len() {
                    break;
                }
                dst[at..at + width_bytes].copy_from_slice(src);
            }
        }

        let fb = self.buffers[back].fb;
        let plane = smithay::backend::drm::PlaneState {
            handle: self.surface.plane(),
            config: Some(smithay::backend::drm::PlaneConfig {
                src: Rectangle::from_size((f64::from(self.size.w), f64::from(self.size.h)).into()),
                dst: Rectangle::from_size(self.size),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb,
                fence: None,
            }),
        };
        if self.modeset_done {
            self.surface.page_flip([plane], true).context("page flip")?;
        } else {
            self.surface.commit([plane], true).context("modeset")?;
            self.modeset_done = true;
        }
        self.front = back;
        Ok(())
    }

    /// After a session pause the CRTC's state is not ours; commit again.
    pub(super) fn reset(&mut self) {
        self.modeset_done = false;
        if let Err(e) = self.surface.reset_state() {
            tracing::debug!(error = %e, "resetting a dumb surface");
        }
    }

    fn release(&self, fb: DumbFb) {
        let _ = self.fd.destroy_framebuffer(fb.fb);
        let _ = self.fd.destroy_dumb_buffer(fb.buffer);
    }
}

impl Drop for DumbSurface {
    fn drop(&mut self) {
        for fb in &self.buffers {
            let _ = self.fd.destroy_framebuffer(fb.fb);
        }
        // The dumb buffers themselves go with the fd's last reference; the
        // kernel frees them on close, which is the only release `drm` offers
        // for a buffer borrowed out of an array.
    }
}
