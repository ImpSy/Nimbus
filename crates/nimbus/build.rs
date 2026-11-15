//! macOS: cpal's (unused) loopback-capture code references CoreAudio's
//! process-tap API (macOS 14.2+), which older SDKs don't declare. Resolve
//! those symbols at load time instead of at link time, so Nimbus links with
//! whatever SDK `xcode-select` points to.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    for sym in [
        "_AudioHardwareCreateProcessTap",
        "_AudioHardwareDestroyProcessTap",
    ] {
        println!("cargo:rustc-link-arg=-Wl,-U,{sym}");
    }
}
