//! H.264 decoding, tuned for latency: every access unit that goes in comes
//! out as a picture immediately (no reorder buffering).
//!
//! * Apple Silicon: VideoToolbox, called directly. Decoded pictures stay on
//!   the GPU (IOSurface-backed `CVPixelBuffer`) and are sampled by the
//!   renderer without any copy.
//! * Steam Deck: FFmpeg's H.264 decoder with VA-API (software fallback).

use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
mod ffmpeg;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

#[cfg(target_os = "linux")]
pub use ffmpeg::VideoDecoder;
#[cfg(target_os = "macos")]
pub use videotoolbox::{PixelBuffer, VideoDecoder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    /// Y plane + interleaved UV plane.
    Nv12,
    /// Y, U, V planes.
    Yuv420p,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

#[derive(Debug, Clone, Copy)]
pub struct Plane {
    pub offset: usize,
    pub stride: usize,
    pub width: u32,
    pub height: u32,
}

/// A picture in system memory.
pub struct CpuImage {
    pub layout: PixelLayout,
    pub data: Vec<u8>,
    pub planes: Vec<Plane>,
    pool: Option<BufferPool>,
}

impl CpuImage {
    pub fn new(
        layout: PixelLayout,
        data: Vec<u8>,
        planes: Vec<Plane>,
        pool: Option<BufferPool>,
    ) -> Self {
        Self {
            layout,
            data,
            planes,
            pool,
        }
    }

    pub fn plane(&self, i: usize) -> &[u8] {
        let p = self.planes[i];
        &self.data[p.offset..p.offset + p.stride * p.height as usize]
    }
}

impl Drop for CpuImage {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.put(std::mem::take(&mut self.data));
        }
    }
}

pub enum FrameImage {
    Cpu(CpuImage),
    /// Decoded picture still in GPU-accessible memory (macOS).
    #[cfg(target_os = "macos")]
    PixelBuffer(PixelBuffer),
}

/// A decoded picture.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub full_range: bool,
    pub matrix: Matrix,
    pub pts: i64,
    pub image: FrameImage,
}

impl VideoFrame {
    pub fn layout(&self) -> PixelLayout {
        match &self.image {
            FrameImage::Cpu(c) => c.layout,
            #[cfg(target_os = "macos")]
            FrameImage::PixelBuffer(_) => PixelLayout::Nv12,
        }
    }
}

/// Recycles frame buffers so steady-state decoding does not allocate.
#[derive(Clone, Default)]
pub struct BufferPool(Arc<Mutex<Vec<Vec<u8>>>>);

impl BufferPool {
    pub fn get(&self, len: usize) -> Vec<u8> {
        let mut v = self.0.lock().unwrap().pop().unwrap_or_default();
        v.clear();
        v.reserve(len);
        v
    }

    fn put(&self, v: Vec<u8>) {
        let mut pool = self.0.lock().unwrap();
        if pool.len() < 6 {
            pool.push(v);
        }
    }
}

/// Iterate over the NAL units of an Annex-B buffer (without start codes).
pub fn annexb_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Strip the start code (3 bytes, plus a leading zero for 4-byte codes).
            let mut e = s - 3;
            if e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(s, e)| e > s)
        .map(move |(s, e)| &data[s..e])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annexb() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let nals: Vec<&[u8]> = annexb_nals(&data).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]
        );
    }
}
