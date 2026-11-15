//! Decodes clips generated with the FFmpeg command line tool (installed by
//! mise; tests are skipped if it is unavailable). Needs the platform decoder
//! (VideoToolbox on macOS, VA-API or software on Linux).

use std::path::PathBuf;
use std::process::Command;

use nimbus_media::audio::OpusDecoder;
#[cfg(target_os = "macos")]
use nimbus_media::video::BufferPool;
use nimbus_media::video::{CpuImage, FrameImage, PixelLayout, VideoDecoder, VideoFrame};

fn make_clip(name: &str, args: &[&str]) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("nimbus-media-tests");
    std::fs::create_dir_all(&dir).ok()?;
    let out = dir.join(name);
    if out.exists() {
        return Some(out);
    }
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(&out)
        .status()
        .ok()?;
    status.success().then_some(out)
}

/// Split an Annex-B stream into access units on AUD NALs (type 9).
pub fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let sc4 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        if sc3 || sc4 {
            let hdr = if sc4 { i + 4 } else { i + 3 };
            if data[hdr] & 0x1f == 9 {
                starts.push(i);
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    starts.push(data.len());
    starts.windows(2).map(|w| &data[w[0]..w[1]]).collect()
}

fn to_cpu(frame: &VideoFrame) -> Option<&CpuImage> {
    match &frame.image {
        FrameImage::Cpu(c) => Some(c),
        #[cfg(target_os = "macos")]
        FrameImage::PixelBuffer(_) => None,
    }
}

#[test]
fn decode_clip() {
    let Some(path) = make_clip(
        "main_1080p.h264",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1920x1080:rate=60",
            "-t",
            "1",
            "-c:v",
            "libx264",
            "-profile:v",
            "main",
            "-tune",
            "zerolatency",
            "-x264-params",
            "aud=1:keyint=30:bframes=0",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
        ],
    ) else {
        eprintln!("ffmpeg CLI not available, skipping");
        return;
    };
    let data = std::fs::read(path).unwrap();
    let aus = split_access_units(&data);
    assert!(aus.len() >= 59, "got {} access units", aus.len());

    let mut dec = VideoDecoder::new().unwrap();
    eprintln!("backend: {}", dec.backend());
    #[cfg(target_os = "macos")]
    let pool = BufferPool::default();
    let mut frames = 0usize;
    for (i, au) in aus.iter().enumerate() {
        let f = dec
            .decode(au, i as i64)
            .unwrap()
            .expect("low delay: one picture per access unit");
        assert_eq!((f.width, f.height), (1920, 1080));
        assert_eq!(f.pts, i as i64, "output must match input order");
        assert!(!f.full_range);
        // The picture must contain the test pattern, not black/green.
        #[cfg(target_os = "macos")]
        let owned = match &f.image {
            FrameImage::PixelBuffer(pb) => Some(pb.copy_to_cpu(&pool)),
            _ => None,
        };
        #[cfg(not(target_os = "macos"))]
        let owned: Option<CpuImage> = None;
        let cpu = to_cpu(&f).or(owned.as_ref()).expect("picture data");
        if i == 10 {
            let y = cpu.plane(0);
            let mean = y.iter().map(|&v| v as u64).sum::<u64>() / y.len() as u64;
            assert!((40..220).contains(&mean), "suspicious luma mean {mean}");
            assert_eq!(
                cpu.layout,
                if cfg!(target_os = "macos") {
                    PixelLayout::Nv12
                } else {
                    cpu.layout
                }
            );
        }
        frames += 1;
    }
    assert_eq!(frames, aus.len());
}

#[test]
fn opus_decodes() {
    let Some(path) = make_clip(
        "tone.opus",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-ac",
            "2",
            "-c:a",
            "libopus",
            "-f",
            "ogg",
        ],
    ) else {
        return;
    };
    // Minimal Ogg page parser to pull out Opus packets.
    let data = std::fs::read(path).unwrap();
    let mut packets = Vec::new();
    let mut i = 0;
    let mut cur = Vec::new();
    while i + 27 <= data.len() && &data[i..i + 4] == b"OggS" {
        let nseg = data[i + 26] as usize;
        let lacing = &data[i + 27..i + 27 + nseg];
        let mut off = i + 27 + nseg;
        for &l in lacing {
            cur.extend_from_slice(&data[off..off + l as usize]);
            off += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        i = off;
    }
    let mut dec = OpusDecoder::new().unwrap();
    let mut out = Vec::new();
    let mut total = 0;
    for p in packets.iter().skip(2) {
        total += dec.decode(p, &mut out).unwrap();
    }
    assert!(total > 40_000, "decoded {total} samples");
    let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "peak {peak}");
}

/// `cargo test -p nimbus-media --release bench -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_decoders() {
    let Some(path) = make_clip(
        "bench_1080p_20mbps.h264",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1920x1080:rate=60,noise=alls=12:allf=t",
            "-t",
            "4",
            "-c:v",
            "libx264",
            "-profile:v",
            "main",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-b:v",
            "20M",
            "-maxrate",
            "20M",
            "-bufsize",
            "2M",
            "-x264-params",
            "aud=1:keyint=120:bframes=0",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
        ],
    ) else {
        return;
    };
    let data = std::fs::read(path).unwrap();
    let aus = split_access_units(&data);
    {
        let mut dec = VideoDecoder::new().unwrap();
        let mut times = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            let t = std::time::Instant::now();
            let f = dec.decode(au, i as i64).unwrap();
            assert!(f.is_some());
            if i >= 10 {
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
        eprintln!(
            "{:>12}: median {:.2} ms, p95 {:.2} ms, max {:.2} ms ({} frames, {:.1} MB)",
            dec.backend(),
            p(0.5),
            p(0.95),
            p(1.0),
            aus.len(),
            data.len() as f64 / 1e6
        );
    }
}
