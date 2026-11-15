//! H.264 decoding with VideoToolbox, called directly (no FFmpeg).
//!
//! * frames are decoded synchronously and without temporal processing, so
//!   each access unit produces its picture before `decode` returns;
//! * output buffers are IOSurface-backed and Metal compatible: the renderer
//!   wraps them as Metal textures (see [`MetalTextureCache`]) instead of
//!   copying pixels around.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, kCFAllocatorNull,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{
    CVImageBuffer, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSessionSetProperty, kVTDecompressionPropertyKey_RealTime,
};

use super::{FrameImage, Matrix, PixelLayout, Plane, VideoFrame, annexb_nals};
use crate::MediaError;

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;
const NAL_FILLER: u8 = 12;
/// kVTInvalidSessionErr: the session must be recreated (e.g. after sleep).
const INVALID_SESSION: i32 = -12903;

/// A decoded picture: an IOSurface-backed NV12 `CVPixelBuffer`.
#[derive(Clone)]
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

// CoreVideo buffers are reference counted with atomic retain/release and
// safe to hand between threads.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl PixelBuffer {
    pub fn raw(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.0) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.0) as u32
    }

    /// Copy the planes to system memory (fallback when zero-copy rendering
    /// is unavailable).
    pub fn copy_to_cpu(&self, pool: &super::BufferPool) -> super::CpuImage {
        let pb = &*self.0;
        unsafe {
            CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
        }
        let mut planes = Vec::with_capacity(2);
        let total: usize = (0..2)
            .map(|i| {
                CVPixelBufferGetBytesPerRowOfPlane(pb, i) * CVPixelBufferGetHeightOfPlane(pb, i)
            })
            .sum();
        let mut data = pool.get(total);
        for i in 0..2 {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
            let height = CVPixelBufferGetHeightOfPlane(pb, i);
            let width = CVPixelBufferGetWidthOfPlane(pb, i);
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *const u8;
            let offset = data.len();
            if !base.is_null() {
                data.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(base, stride * height)
                });
            }
            planes.push(Plane {
                offset,
                stride,
                width: width as u32,
                height: height as u32,
            });
        }
        unsafe {
            CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
        }
        super::CpuImage::new(PixelLayout::Nv12, data, planes, Some(pool.clone()))
    }
}

/// Output of the decompression callback (stable heap location, passed as
/// the callback's refcon).
#[derive(Default)]
struct Output {
    status: i32,
    frame: Option<PixelBuffer>,
}

unsafe extern "C-unwind" fn on_decoded(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    let out = unsafe { &mut *(refcon as *mut Output) };
    out.status = status;
    if status == 0
        && let Some(image) = NonNull::new(image)
    {
        out.frame = Some(PixelBuffer(unsafe { CFRetained::retain(image) }));
    }
}

pub struct VideoDecoder {
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    output: Box<Output>,
    sample: Vec<u8>,
    consecutive_errors: u32,
}

// The session is only used from the decode thread that owns the decoder.
unsafe impl Send for VideoDecoder {}

fn cf(t: &impl AsRef<CFType>) -> &CFType {
    t.as_ref()
}

impl VideoDecoder {
    pub fn new() -> Result<Self, MediaError> {
        Ok(Self {
            session: None,
            format: None,
            sps: Vec::new(),
            pps: Vec::new(),
            output: Box::default(),
            sample: Vec::with_capacity(512 * 1024),
            consecutive_errors: 0,
        })
    }

    pub fn backend(&self) -> &'static str {
        "VideoToolbox"
    }

    pub fn is_hardware(&self) -> bool {
        true
    }

    pub fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors
    }

    fn create_session(&mut self) -> Result<(), MediaError> {
        self.session = None;
        self.format = None;
        let sets = [self.sps.as_ptr(), self.pps.as_ptr()];
        let sizes = [self.sps.len(), self.pps.len()];
        let mut format: *const CMFormatDescription = ptr::null();
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                2,
                NonNull::new(sets.as_ptr() as *mut NonNull<u8>).unwrap(),
                NonNull::new(sizes.as_ptr() as *mut usize).unwrap(),
                4,
                NonNull::from(&mut format),
            )
        };
        if status != 0 || format.is_null() {
            return Err(MediaError::Decode(format!("bad SPS/PPS ({status})")));
        }
        let format = unsafe {
            CFRetained::from_raw(NonNull::new(format as *mut CMFormatDescription).unwrap())
        };

        // NV12, IOSurface backed, Metal compatible: lets the renderer sample
        // the decoded picture directly.
        let pixel_formats = objc2_core_foundation::CFArray::from_retained_objects(&[
            CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32),
            CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange as i32),
        ]);
        let iosurface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        let keys: [&CFString; 3] = unsafe {
            [
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferMetalCompatibilityKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            ]
        };
        let values: [&CFType; 3] = [
            cf(&*pixel_formats),
            cf(CFBoolean::new(true)),
            cf(&*iosurface),
        ];
        let attributes = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);

        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_decoded),
            decompressionOutputRefCon: &mut *self.output as *mut Output as *mut c_void,
        };
        let mut session: *mut VTDecompressionSession = ptr::null_mut();
        let video_format: &CMVideoFormatDescription = &format;
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                video_format,
                None,
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut session),
            )
        };
        if status != 0 || session.is_null() {
            return Err(MediaError::Init(format!(
                "VTDecompressionSessionCreate failed ({status})"
            )));
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new(session).unwrap()) };
        unsafe {
            VTSessionSetProperty(
                session.as_ref(),
                kVTDecompressionPropertyKey_RealTime,
                Some(cf(CFBoolean::new(true))),
            );
        }
        tracing::info!("VideoToolbox session created");
        self.format = Some(format);
        self.session = Some(session);
        Ok(())
    }

    /// Decode one Annex-B access unit. Returns the picture (always, for a
    /// decodable frame) or `None` while waiting for SPS/PPS.
    pub fn decode(&mut self, data: &[u8], pts: i64) -> Result<Option<VideoFrame>, MediaError> {
        self.sample.clear();
        let mut new_sps = None;
        let mut new_pps = None;
        for nal in annexb_nals(data) {
            match nal[0] & 0x1f {
                NAL_SPS => new_sps = Some(nal),
                NAL_PPS => new_pps = Some(nal),
                NAL_AUD | NAL_FILLER => {}
                _ => {
                    // AVCC: 4-byte big-endian length prefix.
                    self.sample
                        .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    self.sample.extend_from_slice(nal);
                }
            }
        }
        let mut params_changed = false;
        if let Some(s) = new_sps
            && s != self.sps.as_slice()
        {
            self.sps = s.to_vec();
            params_changed = true;
        }
        if let Some(p) = new_pps
            && p != self.pps.as_slice()
        {
            self.pps = p.to_vec();
            params_changed = true;
        }
        if (params_changed || self.session.is_none())
            && !self.sps.is_empty()
            && !self.pps.is_empty()
        {
            self.create_session()?;
        }
        let (Some(session), Some(format)) = (&self.session, &self.format) else {
            return Ok(None);
        };
        if self.sample.is_empty() {
            return Ok(None);
        }

        let len = self.sample.len();
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                self.sample.as_mut_ptr() as *mut c_void,
                len,
                kCFAllocatorNull,
                ptr::null(),
                0,
                len,
                0,
                NonNull::from(&mut block),
            )
        };
        if status != 0 || block.is_null() {
            return Err(MediaError::Decode(format!("CMBlockBuffer ({status})")));
        }
        let block = unsafe { CFRetained::from_raw(NonNull::new(block).unwrap()) };
        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(format),
                1,
                0,
                ptr::null(),
                1,
                &len,
                NonNull::from(&mut sample),
            )
        };
        if status != 0 || sample.is_null() {
            return Err(MediaError::Decode(format!("CMSampleBuffer ({status})")));
        }
        let sample = unsafe { CFRetained::from_raw(NonNull::new(sample).unwrap()) };

        self.output.frame = None;
        self.output.status = 0;
        let mut info = VTDecodeInfoFlags(0);
        // Synchronous decode, no temporal processing: the output callback
        // runs before this returns.
        let status = unsafe {
            session.decode_frame(&sample, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info)
        };
        if status == INVALID_SESSION {
            tracing::warn!("VideoToolbox session invalidated, recreating");
            self.session = None;
            self.consecutive_errors += 1;
            return Err(MediaError::Decode("decoder session lost".into()));
        }
        if status != 0 || self.output.status != 0 {
            self.consecutive_errors += 1;
            return Err(MediaError::Decode(format!(
                "VTDecompressionSessionDecodeFrame ({status} / {})",
                self.output.status
            )));
        }
        let Some(frame) = self.output.frame.take() else {
            // Dropped by the decoder (e.g. waiting for a keyframe).
            return Ok(None);
        };
        self.consecutive_errors = 0;

        let pb = frame.raw();
        let format_type = CVPixelBufferGetPixelFormatType(pb);
        let full_range = format_type == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
        // CVBufferGetAttachment (not the macOS 12+ Copy variant) so this links
        // against older SDKs too.
        #[allow(deprecated)]
        let matrix = unsafe { pb.get_attachment(kCVImageBufferYCbCrMatrixKey, ptr::null_mut()) }
            .and_then(|v| v.downcast::<CFString>().ok())
            .map(|m| {
                if *m == *unsafe { kCVImageBufferYCbCrMatrix_ITU_R_601_4 } {
                    Matrix::Bt601
                } else {
                    Matrix::Bt709
                }
            })
            .unwrap_or(Matrix::Bt709);
        Ok(Some(VideoFrame {
            width: frame.width(),
            height: frame.height(),
            full_range,
            matrix,
            pts,
            image: FrameImage::PixelBuffer(frame),
        }))
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            unsafe { s.invalidate() };
        }
    }
}

/// Wraps decoded `CVPixelBuffer` planes as Metal textures (zero copy).
pub struct MetalTextureCache(CFRetained<CVMetalTextureCache>);

unsafe impl Send for MetalTextureCache {}
unsafe impl Sync for MetalTextureCache {}

/// A plane of a decoded picture as a Metal texture. Keep this alive until
/// the GPU is done sampling it.
pub struct MetalPlane {
    pub texture: Retained<ProtocolObject<dyn MTLTexture>>,
    pub width: u32,
    pub height: u32,
    _cv_texture: CFRetained<CVMetalTexture>,
    _buffer: PixelBuffer,
}

unsafe impl Send for MetalPlane {}
unsafe impl Sync for MetalPlane {}

impl MetalTextureCache {
    pub fn new(device: &ProtocolObject<dyn MTLDevice>) -> Option<Self> {
        let mut cache: *mut CVMetalTextureCache = ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create(None, None, device, None, NonNull::from(&mut cache))
        };
        if status != 0 || cache.is_null() {
            tracing::warn!("CVMetalTextureCacheCreate failed ({status})");
            return None;
        }
        Some(Self(unsafe {
            CFRetained::from_raw(NonNull::new(cache).unwrap())
        }))
    }

    /// Luma (R8) and chroma (RG8) planes of an NV12 picture.
    pub fn planes(&self, buffer: &PixelBuffer) -> Option<[MetalPlane; 2]> {
        let pb = buffer.raw();
        let make = |plane: usize, format: MTLPixelFormat| -> Option<MetalPlane> {
            let width = CVPixelBufferGetWidthOfPlane(pb, plane);
            let height = CVPixelBufferGetHeightOfPlane(pb, plane);
            let mut tex: *mut CVMetalTexture = ptr::null_mut();
            let status = unsafe {
                CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.0,
                    pb,
                    None,
                    format,
                    width,
                    height,
                    plane,
                    NonNull::from(&mut tex),
                )
            };
            if status != 0 || tex.is_null() {
                tracing::debug!(
                    plane,
                    status,
                    "CVMetalTextureCacheCreateTextureFromImage failed"
                );
                return None;
            }
            let cv_texture = unsafe { CFRetained::from_raw(NonNull::new(tex).unwrap()) };
            #[allow(deprecated)]
            let texture = CVMetalTextureGetTexture(&cv_texture)?;
            Some(MetalPlane {
                texture,
                width: width as u32,
                height: height as u32,
                _cv_texture: cv_texture,
                _buffer: buffer.clone(),
            })
        };
        Some([
            make(0, MTLPixelFormat::R8Unorm)?,
            make(1, MTLPixelFormat::RG8Unorm)?,
        ])
    }
}
