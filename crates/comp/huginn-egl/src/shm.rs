//! Writing pixels into a client's `wl_shm` buffer.
//!
//! The capture protocol (`raven_capture_v1`) has the client supply the memory
//! a frame is drawn into, as every Wayland capture protocol does. Smithay
//! exposes a shm buffer's memory only as a raw pointer — the pool is shared
//! with the client, which may write to it at any moment, so a slice into it
//! would be a promise Rust cannot keep — and copying through a raw pointer is
//! `unsafe`. This is the one place that does it.

use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::wayland::shm::{BufferAccessError, with_buffer_contents_mut};

/// Why a copy into a shm buffer did not happen.
#[derive(Debug, thiserror::Error)]
pub enum ShmWriteError {
    /// Not a shm buffer, or its pool could not be mapped. Smithay has already
    /// posted a protocol error for a pool the client lied about the size of.
    #[error("the buffer cannot be written: {0}")]
    Access(#[from] BufferAccessError),
    /// The rows asked for do not fit the buffer, or the source is shorter
    /// than they claim. Nothing was written.
    #[error("the pixels do not fit the buffer")]
    OutOfBounds,
}

/// Copy `rows` rows of `row_bytes` bytes into the shm buffer `buffer`, top
/// row first. Row `n` is read from `src[n * src_stride..][..row_bytes]` and
/// lands at the buffer's own offset plus `n` times its own stride, so a
/// client that pads its rows gets them padded.
///
/// Every bound is checked before a byte is written: the source against its
/// length, the rows against the buffer's height and stride, and the last
/// byte against the length of the pool. A buffer that does not fit is left
/// untouched.
pub fn write_shm_rows(
    buffer: &WlBuffer,
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<(), ShmWriteError> {
    if rows == 0 || row_bytes == 0 {
        return Ok(());
    }
    // The last source row must end inside `src`.
    let src_end = (rows - 1)
        .checked_mul(src_stride)
        .and_then(|start| start.checked_add(row_bytes))
        .ok_or(ShmWriteError::OutOfBounds)?;
    if src_end > src.len() || row_bytes > src_stride {
        return Err(ShmWriteError::OutOfBounds);
    }

    with_buffer_contents_mut(buffer, |ptr, len, data| {
        let offset = usize::try_from(data.offset).map_err(|_| ShmWriteError::OutOfBounds)?;
        let stride = usize::try_from(data.stride).map_err(|_| ShmWriteError::OutOfBounds)?;
        let height = usize::try_from(data.height).map_err(|_| ShmWriteError::OutOfBounds)?;
        if rows > height || row_bytes > stride {
            return Err(ShmWriteError::OutOfBounds);
        }
        // One past the last byte written, which must be inside the pool.
        let end = (rows - 1)
            .checked_mul(stride)
            .and_then(|last| last.checked_add(offset))
            .and_then(|last| last.checked_add(row_bytes))
            .ok_or(ShmWriteError::OutOfBounds)?;
        if end > len {
            return Err(ShmWriteError::OutOfBounds);
        }
        for row in 0..rows {
            let from = &src[row * src_stride..row * src_stride + row_bytes];
            let at = offset + row * stride;
            // SAFETY: `with_buffer_contents_mut` hands the callback a pointer
            // to the start of the pool's mapping, valid for reads and writes
            // of `len` bytes for as long as the callback runs; smithay maps
            // the pool PROT_READ | PROT_WRITE, MAP_SHARED. The destination
            // range `at..at + row_bytes` ends at or before `end`, checked
            // against `len` above, so every byte written is inside the
            // mapping. The source is a slice of our own memory, a heap
            // allocation, and cannot overlap the pool's mmap region, so the
            // ranges do not overlap. No reference into the shared memory is
            // ever created — the copy goes through the raw pointer — which is
            // the one thing smithay's contract rules out, since the client
            // may be writing the same pages. A client that truncates its
            // memfd under us raises SIGBUS, which smithay's handler turns
            // into an error return rather than a crash.
            unsafe {
                std::ptr::copy_nonoverlapping(from.as_ptr(), ptr.add(at), row_bytes);
            }
        }
        Ok(())
    })?
}
