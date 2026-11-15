//! When linking the static FFmpeg built by `scripts/build-ffmpeg.sh`
//! (`FFMPEG_DIR`), also link the system libraries / frameworks it depends on
//! (VideoToolbox & co. on macOS, libva / libdrm on Linux). FFmpeg records them
//! in its pkg-config files (`Libs:` for static builds, `Libs.private:` for
//! shared ones).
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if std::env::var("CARGO_FEATURE_STATIC").is_err() {
        return;
    }
    let Ok(dir) = std::env::var("FFMPEG_DIR") else {
        return;
    };
    let pc_dir = Path::new(&dir).join("lib/pkgconfig");
    let mut seen = std::collections::HashSet::new();
    for lib in ["libavcodec", "libswresample", "libavutil"] {
        let pc = pc_dir.join(format!("{lib}.pc"));
        println!("cargo:rerun-if-changed={}", pc.display());
        let Ok(text) = std::fs::read_to_string(&pc) else {
            continue;
        };
        for line in text.lines() {
            let Some(rest) = line
                .strip_prefix("Libs.private:")
                .or_else(|| line.strip_prefix("Libs:"))
            else {
                continue;
            };
            let mut tokens = rest.split_whitespace();
            while let Some(t) = tokens.next() {
                let directive = if t == "-framework" {
                    tokens
                        .next()
                        .map(|f| format!("cargo:rustc-link-lib=framework={f}"))
                } else if let Some(l) = t.strip_prefix("-l") {
                    // Our own static libs are linked by ffmpeg-sys-next.
                    (!l.starts_with("av") && !l.starts_with("sw"))
                        .then(|| format!("cargo:rustc-link-lib={l}"))
                } else {
                    // FFmpeg's own lib dir is already on the search path.
                    t.strip_prefix("-L")
                        .filter(|p| !p.starts_with(dir.as_str()) && !p.starts_with("${"))
                        .map(|p| format!("cargo:rustc-link-search=native={p}"))
                };
                if let Some(d) = directive
                    && seen.insert(d.clone())
                {
                    println!("{d}");
                }
            }
        }
    }
}
