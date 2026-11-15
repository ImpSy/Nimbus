//! Opus decoding (FFmpeg's native decoder, no libopus needed).

use std::ptr;

use ffmpeg_sys_next as ff;

use crate::MediaError;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

pub struct OpusDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    pad: Vec<u8>,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, MediaError> {
        crate::init_ffmpeg();
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_OPUS);
            if codec.is_null() {
                return Err(MediaError::Init("no Opus decoder in FFmpeg".into()));
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            (*ctx).sample_rate = SAMPLE_RATE as i32;
            ff::av_channel_layout_default(&mut (*ctx).ch_layout, CHANNELS as i32);
            let ret = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                let mut ctx = ctx;
                ff::avcodec_free_context(&mut ctx);
                return Err(MediaError::Init(format!("opus open failed ({ret})")));
            }
            Ok(Self {
                ctx,
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                pad: Vec::with_capacity(4096),
            })
        }
    }

    /// Decode one packet, appending interleaved stereo f32 samples to `out`.
    /// Returns the number of frames (samples per channel) decoded.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<usize, MediaError> {
        self.pad.clear();
        self.pad.extend_from_slice(data);
        self.pad
            .resize(data.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
        let mut produced = 0;
        unsafe {
            (*self.pkt).data = self.pad.as_mut_ptr();
            (*self.pkt).size = data.len() as i32;
            let ret = ff::avcodec_send_packet(self.ctx, self.pkt);
            (*self.pkt).data = ptr::null_mut();
            (*self.pkt).size = 0;
            if ret < 0 {
                return Err(MediaError::Decode(format!("opus send_packet ({ret})")));
            }
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx, self.frame);
                if ret == ff::AVERROR(libc::EAGAIN) || ret == ff::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    return Err(MediaError::Decode(format!("opus receive_frame ({ret})")));
                }
                let f = &*self.frame;
                let n = f.nb_samples as usize;
                let ch = f.ch_layout.nb_channels.max(1) as usize;
                out.reserve(n * CHANNELS as usize);
                if f.format == ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32 {
                    let l = std::slice::from_raw_parts(f.data[0] as *const f32, n);
                    let r = if ch > 1 {
                        std::slice::from_raw_parts(f.data[1] as *const f32, n)
                    } else {
                        l
                    };
                    for i in 0..n {
                        out.push(l[i]);
                        out.push(r[i]);
                    }
                } else if f.format == ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as i32 {
                    let s = std::slice::from_raw_parts(f.data[0] as *const f32, n * ch);
                    for i in 0..n {
                        out.push(s[i * ch]);
                        out.push(s[i * ch + (ch > 1) as usize]);
                    }
                } else if f.format == ff::AVSampleFormat::AV_SAMPLE_FMT_S16 as i32 {
                    let s = std::slice::from_raw_parts(f.data[0] as *const i16, n * ch);
                    for i in 0..n {
                        out.push(s[i * ch] as f32 / 32768.0);
                        out.push(s[i * ch + (ch > 1) as usize] as f32 / 32768.0);
                    }
                }
                produced += n;
                ff::av_frame_unref(self.frame);
            }
        }
        Ok(produced)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
