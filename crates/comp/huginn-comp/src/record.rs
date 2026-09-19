//! Screen recording, done by the compositor itself.
//!
//! # Why this is in the compositor
//!
//! For the reason screenshots are (see [`crate::screenshot`]): Huginn offers
//! no capture protocol and holds the framebuffer alone, so the one process
//! that can record the screen is the one drawing it. `Super`+`Print` starts a
//! recording of the focused screen and stops it again.
//!
//! # How it works
//!
//! Every [`INTERVAL`] — thirty times a second — the recording asks whether its
//! screen has drawn anything new since the last capture. If not, there is
//! nothing to do: an idle desktop records no frames, and the file says so by
//! the gap between two timestamps. If so, the scene is rendered once more
//! offscreen, pointer included and the recording dot left out, and a
//! read-back is queued.
//!
//! The read-back is collected on the *next* tick, not this one. GL fills the
//! pixel buffer behind the draw; mapping it straight away would stall the
//! frame loop until the GPU caught up, and mapping it a tick later finds it
//! long since ready. The pixels go to a writer thread, which compresses them
//! into the Raven recording format ([`raven_rec`]) and appends them to the
//! file. When the writer falls behind, the frame is dropped rather than
//! queued: a recording that skips a frame is better than a desktop that
//! stutters because it is being recorded.
//!
//! Files land in `<videos>/Recordings`, named `Recording-YYYY-MM-DD-HHMMSS.rvr`.
//! The format is lossless and not something a video player opens; turning a
//! recording into one is a separate step, outside the compositor.
//!
//! # What ends a recording
//!
//! `Super`+`Print` again, or the compositor quitting — both write the end
//! marker. Unplugging the screen or changing its resolution ends it too, since
//! a recording is one size for its whole length. A crash leaves a file with no
//! end marker, which reads back up to the last whole frame.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use smithay::{
    backend::renderer::{
        ExportMem,
        gles::{GlesMapping, GlesRenderer, GlesTexture},
    },
    utils::{Physical, Size},
};

use huginn_core::geometry::Rect;

use crate::canvas::{Canvas, Panel};
use crate::pointer::Cursor;
use crate::state::{Huginn, OutputInfo};

/// How often a recording looks for a new frame: 30 Hz.
///
/// Not the screen's refresh rate, on purpose. Half of 60 Hz is smooth for
/// anything a desktop does, it halves what the writer has to keep up with, and
/// the machine this has to work on is a low-power one.
pub(crate) const INTERVAL: Duration = Duration::from_micros(1_000_000 / 30);

/// Frames allowed to wait for the writer. Two is a frame of slack for a
/// momentary stall on the disk; a writer further behind than that is not
/// catching up, and holding more would only spend memory — eight megabytes a
/// frame at 1080p — on proving it.
const QUEUE: usize = 2;

/// What the compositor hands the writer thread.
enum Message {
    /// A frame, shown at this long into the recording.
    Frame(Duration, Vec<u8>),
    /// The recording stopped this long in: write the end marker and finish.
    Finish(Duration),
}

/// A recording in progress.
pub(crate) struct Recording {
    /// The connector name of the screen being recorded. A name rather than an
    /// index, because indices shift when another screen is unplugged.
    output: String,
    size: Size<i32, Physical>,
    path: PathBuf,
    started: Instant,
    /// Drawn into on every capture, created once.
    texture: GlesTexture,
    /// A read-back queued on the last tick, and when that frame was drawn.
    in_flight: Option<(Duration, GlesMapping)>,
    /// The screen has drawn something new since the last capture.
    damaged: bool,
    frames: SyncSender<Message>,
    /// Frame buffers the writer is finished with, to be filled again rather
    /// than allocated again.
    spare: Receiver<Vec<u8>>,
    writer: JoinHandle<Result<u64>>,
    dropped: u64,
}

/// What a finished recording amounted to.
pub(crate) struct Summary {
    pub(crate) path: PathBuf,
    pub(crate) frames: u64,
    pub(crate) dropped: u64,
    pub(crate) length: Duration,
}

impl Recording {
    /// Start recording screen `output`.
    pub(crate) fn start(
        renderer: &mut GlesRenderer,
        state: &Huginn,
        output: usize,
    ) -> Result<Self> {
        let info = state
            .outputs()
            .get(output)
            .context("recording a screen that is not connected")?;
        let size = mode_size(info).context("the screen has no mode, so nothing to record")?;
        let (width, height) = (
            u32::try_from(size.w).context("a screen of negative width")?,
            u32::try_from(size.h).context("a screen of negative height")?,
        );
        // Before the file, so a GPU that cannot do this leaves nothing behind.
        let texture = crate::screenshot::offscreen_texture(renderer, size)?;

        let dir = crate::userdirs::user_dir("XDG_VIDEOS_DIR", "Videos")
            .map(|base| base.join("Recordings"))
            .context("no directory to save a recording in (no HOME, no XDG_VIDEOS_DIR)")?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let stem = crate::userdirs::timestamp("Recording");
        let path = crate::userdirs::unique_path(&dir, &stem, raven_rec::EXTENSION);
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let encoder = raven_rec::Encoder::new(BufWriter::new(file), width, height)
            .with_context(|| format!("starting {}", path.display()))?;

        let (frames, inbox) = mpsc::sync_channel(QUEUE);
        let (give_back, spare) = mpsc::channel();
        let writer = thread::Builder::new()
            .name("huginn-record".to_owned())
            .spawn(move || write(encoder, inbox, give_back))
            .context("starting the recording writer")?;

        Ok(Self {
            output: info.name.clone(),
            size,
            path,
            started: Instant::now(),
            texture,
            in_flight: None,
            // The first tick captures whatever is on screen already.
            damaged: true,
            frames,
            spare,
            writer,
            dropped: 0,
        })
    }

    /// The connector name of the screen being recorded.
    pub(crate) fn output(&self) -> &str {
        &self.output
    }

    /// Where the recording is being written.
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The recorded screen presented a new frame.
    pub(crate) fn note_damage(&mut self) {
        self.damaged = true;
    }

    /// Collect the last capture and, if the screen has changed since, queue
    /// the next. An error means the recording cannot go on; stop it.
    pub(crate) fn tick(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &Huginn,
        cursor: Option<&Cursor>,
    ) -> Result<()> {
        self.collect(renderer)?;

        let info = state
            .outputs()
            .iter()
            .find(|info| info.name == self.output)
            .context("the screen being recorded was unplugged")?;
        if mode_size(info) != Some(self.size) {
            bail!("the screen being recorded changed resolution");
        }
        if !std::mem::take(&mut self.damaged) {
            return Ok(());
        }

        let scale = info.scale.fractional();
        let elements = crate::render::recording_elements(renderer, state, cursor, info.rect, scale);
        let mapping = crate::screenshot::draw_offscreen(
            renderer,
            &mut self.texture,
            &elements,
            self.size,
            scale,
            false,
        )?;
        self.in_flight = Some((self.started.elapsed(), mapping));
        Ok(())
    }

    /// Stop recording: keep the frame still on its way back from the GPU,
    /// write the end marker, and wait for the writer to finish.
    ///
    /// Waits on the writer thread, which has at most [`QUEUE`] frames left to
    /// compress; that is the only moment a recording holds up the compositor.
    pub(crate) fn stop(mut self, renderer: &mut GlesRenderer) -> Result<Summary> {
        let collected = self.collect(renderer);
        let length = self.started.elapsed();
        let Recording {
            frames,
            writer,
            path,
            dropped,
            ..
        } = self;
        // If the writer has already given up this fails, and joining it below
        // says why, which is the more useful error.
        let _ = frames.send(Message::Finish(length));
        drop(frames);
        let written = writer
            .join()
            .map_err(|_| anyhow::anyhow!("the recording writer panicked"))??;
        collected?;
        Ok(Summary {
            path,
            frames: written,
            dropped,
            length,
        })
    }

    /// Hand the queued read-back, if there is one, to the writer.
    fn collect(&mut self, renderer: &mut GlesRenderer) -> Result<()> {
        let Some((pts, mapping)) = self.in_flight.take() else {
            return Ok(());
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| anyhow::anyhow!("mapping a recorded frame: {e}"))?;
        let mut buffer = self.spare.try_recv().unwrap_or_default();
        buffer.clear();
        buffer.extend_from_slice(pixels);
        match self.frames.try_send(Message::Frame(pts, buffer)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => bail!("the recording writer stopped"),
        }
    }
}

/// The writer thread: compress each frame into the file as it arrives.
///
/// Returns how many frames it wrote. The loop also ends if the compositor goes
/// away without a [`Message::Finish`], in which case the file is flushed
/// without an end marker — it reads as cut short, which it was.
fn write(
    mut encoder: raven_rec::Encoder<BufWriter<File>>,
    inbox: Receiver<Message>,
    give_back: Sender<Vec<u8>>,
) -> Result<u64> {
    let mut written = 0;
    for message in inbox {
        match message {
            Message::Frame(pts, rgba) => {
                encoder.push(pts, &rgba).context("writing a frame")?;
                written += 1;
                // Nobody left to take it back is fine; the buffer just goes.
                let _ = give_back.send(rgba);
            }
            Message::Finish(length) => {
                encoder.finish(length).context("finishing the recording")?;
                return Ok(written);
            }
        }
    }
    encoder.flush().context("flushing the recording")?;
    Ok(written)
}

/// The screen's mode, in physical pixels and turned the way the screen is:
/// the size a recording of it is.
fn mode_size(info: &OutputInfo) -> Option<Size<i32, Physical>> {
    info.frame_size()
}

/// Diameter of the recording dot, in logical pixels.
const DOT: usize = 10;
/// The dark ring around the dot, so it shows on a red or white background.
const HALO: usize = 2;
/// How far the dot sits in from the screen's top and right edges.
const INSET: i32 = 10;

/// The dot that says a screen is being recorded, drawn at `density`.
///
/// Red, round, and still. It does not pulse: a pulse is a frame every
/// sixtieth of a second on a desktop that would otherwise draw none, and every
/// one of those frames would be one more for the recording to capture.
pub(crate) fn indicator(density: u32) -> Panel {
    Panel::from_canvas(&compose_indicator(density), density)
}

fn compose_indicator(density: u32) -> Canvas {
    let d = density.max(1) as usize;
    let side = (DOT + HALO * 2) * d;
    let mut canvas = Canvas::new(side, side);
    canvas.fill_rounded(
        0,
        0,
        side,
        side,
        side as f32 / 2.0,
        crate::theme::BACKGROUND.with_alpha(0xB0),
    );
    canvas.fill_rounded(
        HALO * d,
        HALO * d,
        DOT * d,
        DOT * d,
        (DOT * d) as f32 / 2.0,
        crate::theme::RECORDING,
    );
    canvas
}

/// Where the dot goes on `output`: its top-right corner, inset.
pub(crate) fn indicator_placement(output: Rect, (w, h): (i32, i32)) -> Rect {
    Rect::from_xywh(output.right() - INSET - w, output.y() + INSET, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dot_is_red_in_the_middle_and_clear_in_the_corners() {
        for density in [1, 2] {
            let canvas = compose_indicator(density);
            let side = canvas.stride;
            let pixel = |x: usize, y: usize| {
                let at = (y * side + x) * 4;
                <[u8; 4]>::try_from(&canvas.pixels[at..at + 4]).unwrap()
            };
            let [r, g, b, a] = crate::theme::RECORDING.to_rgba_bytes();
            assert_eq!(pixel(side / 2, side / 2), [r, g, b, a], "density {density}");
            assert_eq!(
                pixel(0, 0)[3],
                0,
                "density {density}: the corner is not clear"
            );
        }
    }

    #[test]
    fn the_dot_sits_in_the_top_right_corner_of_its_screen() {
        // The second of two side-by-side screens.
        let screen = Rect::from_xywh(1920, 0, 1280, 800);
        let rect = indicator_placement(screen, (14, 14));
        assert_eq!(
            rect,
            Rect::from_xywh(1920 + 1280 - INSET - 14, INSET, 14, 14)
        );
    }
}
