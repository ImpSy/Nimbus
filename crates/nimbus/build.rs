//! On macOS, SDL3 (built from source) uses Objective-C `@available` checks,
//! which need `___isPlatformVersionAtLeast` from clang's compiler-rt. rustc
//! links with `-nodefaultlibs`, so add the runtime explicitly.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(out) = std::process::Command::new("clang")
        .arg("--print-resource-dir")
        .output()
    else {
        return;
    };
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let lib = std::path::Path::new(&dir).join("lib/darwin/libclang_rt.osx.a");
    if lib.exists() {
        println!("cargo:rustc-link-arg={}", lib.display());
    }
}
