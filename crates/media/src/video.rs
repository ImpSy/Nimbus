//! H.264 decoding tuned for latency.
//!
//! * hardware decode: VideoToolbox on macOS, VA-API on Linux (Steam Deck),
//!   automatic fallback to software;
//! * `AV_CODEC_FLAG_LOW_DELAY`: a frame comes out for every frame that goes in
//!   (no reorder buffering);
//! * software decode uses slice threading only (frame threading would add one
//!   frame of latency per thread).

use std::ffi::CStr;
use std::ptr;
use std::sync::Arc;

use std::sync::Mutex;

use ffmpeg_sys_next as ff;

use crate::MediaError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    /// Y plane + interleaved UV plane (hardware decoders).
    Nv12,
    /// Y, U, V planes (software decoder).
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

/// A decoded picture in system memory, ready for GPU upload.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub data: Vec<u8>,
    pub planes: Vec<Plane>,
    pub full_range: bool,
    pub matrix: Matrix,
    pub pts: i64,
    pool: Option<BufferPool>,
}

impl VideoFrame {
    pub fn plane(&self, i: usize) -> &[u8] {
        let p = self.planes[i];
        &self.data[p.offset..p.offset + p.stride * p.height as usize]
    }
}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.put(std::mem::take(&mut self.data));
        }
    }
}

/// Recycles frame buffers so steady-state decoding does not allocate.
#[derive(Clone, Default)]
pub struct BufferPool(Arc<Mutex<Vec<Vec<u8>>>>);

impl BufferPool {
    fn get(&self, len: usize) -> Vec<u8> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPreference {
    Auto,
    SoftwareOnly,
}

pub struct VideoDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    hw_device: *mut ff::AVBufferRef,
    hw_format: Option<ff::AVPixelFormat>,
    backend: &'static str,
    pad: Vec<u8>,
    pool: BufferPool,
    consecutive_errors: u32,
}

// The raw pointers are only touched by the owning thread.
unsafe impl Send for VideoDecoder {}

unsafe extern "C" fn pick_hw_format(
    ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        let wanted = (*ctx).opaque as isize as i32;
        let mut p = fmts;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as i32 == wanted {
                return *p;
            }
            p = p.add(1);
        }
        tracing::warn!("hardware pixel format not offered; using software decoding");
        ff::avcodec_default_get_format(ctx, fmts)
    }
}

fn hw_candidates() -> &'static [(ff::AVHWDeviceType, ff::AVPixelFormat, &'static str)] {
    #[cfg(target_os = "macos")]
    {
        &[(
            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            ff::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX,
            "VideoToolbox",
        )]
    }
    #[cfg(target_os = "linux")]
    {
        &[(
            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
            "VA-API",
        )]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

fn av_err(code: i32) -> String {
    let mut buf = [0 as libc::c_char; 128];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

impl VideoDecoder {
    pub fn new(pref: HwPreference) -> Result<Self, MediaError> {
        crate::init_ffmpeg();
        if pref == HwPreference::Auto {
            for &(dev, fmt, name) in hw_candidates() {
                match Self::open(Some((dev, fmt, name))) {
                    Ok(d) => return Ok(d),
                    Err(e) => tracing::warn!("{name} decoder unavailable: {e}"),
                }
            }
        }
        Self::open(None)
    }

    fn open(
        hw: Option<(ff::AVHWDeviceType, ff::AVPixelFormat, &'static str)>,
    ) -> Result<Self, MediaError> {
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                return Err(MediaError::Init("no H.264 decoder in FFmpeg".into()));
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(MediaError::Init("avcodec_alloc_context3 failed".into()));
            }
            let mut dec = VideoDecoder {
                ctx,
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                sw_frame: ff::av_frame_alloc(),
                hw_device: ptr::null_mut(),
                hw_format: None,
                backend: "Software",
                pad: Vec::with_capacity(512 * 1024),
                pool: BufferPool::default(),
                consecutive_errors: 0,
            };
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).flags2 |= ff::AV_CODEC_FLAG2_FAST as i32;

            if let Some((dev_type, fmt, name)) = hw {
                let ret = ff::av_hwdevice_ctx_create(
                    &mut dec.hw_device,
                    dev_type,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                );
                if ret < 0 {
                    return Err(MediaError::Init(format!(
                        "{name} device creation failed: {}",
                        av_err(ret)
                    )));
                }
                (*ctx).hw_device_ctx = ff::av_buffer_ref(dec.hw_device);
                (*ctx).opaque = fmt as i32 as isize as *mut libc::c_void;
                (*ctx).get_format = Some(pick_hw_format);
                (*ctx).thread_count = 1;
                dec.hw_format = Some(fmt);
                dec.backend = name;
            } else {
                let threads = std::thread::available_parallelism()
                    .map(|n| n.get().min(8))
                    .unwrap_or(4);
                (*ctx).thread_count = threads as i32;
                (*ctx).thread_type = ff::FF_THREAD_SLICE;
            }

            let ret = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                return Err(MediaError::Init(format!(
                    "avcodec_open2 failed: {}",
                    av_err(ret)
                )));
            }
            tracing::info!(backend = dec.backend, "H.264 decoder ready");
            Ok(dec)
        }
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    pub fn is_hardware(&self) -> bool {
        self.hw_format.is_some()
    }

    /// Decode one Annex-B access unit. Returns the decoded picture if one
    /// came out (always the case for a valid frame with low-delay).
    pub fn decode(&mut self, data: &[u8], pts: i64) -> Result<Option<VideoFrame>, MediaError> {
        self.pad.clear();
        self.pad.extend_from_slice(data);
        self.pad
            .resize(data.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
        unsafe {
            let pkt = self.pkt;
            (*pkt).data = self.pad.as_mut_ptr();
            (*pkt).size = data.len() as i32;
            (*pkt).pts = pts;
            (*pkt).dts = pts;
            let ret = ff::avcodec_send_packet(self.ctx, pkt);
            (*pkt).data = ptr::null_mut();
            (*pkt).size = 0;
            if ret < 0 && ret != ff::AVERROR(libc::EAGAIN) {
                self.consecutive_errors += 1;
                return Err(MediaError::Decode(av_err(ret)));
            }

            let mut out = None;
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx, self.frame);
                if ret == ff::AVERROR(libc::EAGAIN) || ret == ff::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    self.consecutive_errors += 1;
                    return Err(MediaError::Decode(av_err(ret)));
                }
                // Only keep the newest picture if several come out.
                out = Some(self.extract()?);
                ff::av_frame_unref(self.frame);
            }
            if out.is_some() {
                self.consecutive_errors = 0;
            }
            Ok(out)
        }
    }

    /// Number of consecutive decode errors (used to trigger a software
    /// fallback when a hardware decoder chokes).
    pub fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors
    }

    unsafe fn extract(&mut self) -> Result<VideoFrame, MediaError> {
        unsafe {
            let mut src = self.frame;
            if let Some(hw) = self.hw_format
                && (*src).format == hw as i32
            {
                ff::av_frame_unref(self.sw_frame);
                let ret = ff::av_hwframe_transfer_data(self.sw_frame, src, 0);
                if ret < 0 {
                    return Err(MediaError::Decode(format!(
                        "hw frame transfer failed: {}",
                        av_err(ret)
                    )));
                }
                ff::av_frame_copy_props(self.sw_frame, src);
                src = self.sw_frame;
            }

            let f = &*src;
            let width = f.width as u32;
            let height = f.height as u32;
            let layout = if f.format == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
                PixelLayout::Nv12
            } else if f.format == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
                || f.format == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32
            {
                PixelLayout::Yuv420p
            } else {
                return Err(MediaError::Decode(format!(
                    "unsupported pixel format {}",
                    f.format
                )));
            };
            let full_range = f.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG
                || f.format == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
            let matrix = match f.colorspace {
                ff::AVColorSpace::AVCOL_SPC_BT470BG | ff::AVColorSpace::AVCOL_SPC_SMPTE170M => {
                    Matrix::Bt601
                }
                ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED if height < 720 => Matrix::Bt601,
                _ => Matrix::Bt709,
            };

            let chroma_h = height.div_ceil(2);
            let chroma_w = width.div_ceil(2);
            let plane_dims: Vec<(u32, u32)> = match layout {
                PixelLayout::Nv12 => vec![(width, height), (chroma_w, chroma_h)],
                PixelLayout::Yuv420p => {
                    vec![(width, height), (chroma_w, chroma_h), (chroma_w, chroma_h)]
                }
            };
            let total: usize = plane_dims
                .iter()
                .enumerate()
                .map(|(i, (_, h))| f.linesize[i].unsigned_abs() as usize * *h as usize)
                .sum();
            let mut data = self.pool.get(total);
            let mut planes = Vec::with_capacity(plane_dims.len());
            for (i, (pw, ph)) in plane_dims.iter().enumerate() {
                let stride = f.linesize[i] as usize;
                let len = stride * *ph as usize;
                let offset = data.len();
                data.extend_from_slice(std::slice::from_raw_parts(f.data[i], len));
                planes.push(Plane {
                    offset,
                    stride,
                    width: *pw,
                    height: *ph,
                });
            }

            Ok(VideoFrame {
                width,
                height,
                layout,
                data,
                planes,
                full_range,
                matrix,
                pts: f.pts,
                pool: Some(self.pool.clone()),
            })
        }
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
            if !self.hw_device.is_null() {
                ff::av_buffer_unref(&mut self.hw_device);
            }
        }
    }
}
