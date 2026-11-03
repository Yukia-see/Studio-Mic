use std::path::{Path, PathBuf};
use std::process::Command;

/// SHA256 of librnnoise_ladspa.so from noise-suppression-for-voice v1.10 (same pin as setup-deps.sh).
const RNNOISE_SHA256: &str = "ba2b420c76cd2f1e7684430c96fef8c533041d5f67b94060f8eeda6af1701f97";

fn main() {
    println!("cargo:rerun-if-changed=mic-studio-mockup.html");
    println!("cargo:rerun-if-changed=plugins/speex-ladspa/speex_ladspa.c");
    println!("cargo:rerun-if-changed=assets/plugins/librnnoise_ladspa.so");
    println!("cargo::rustc-check-cfg=cfg(sm_embed_speex)");
    println!("cargo::rustc-check-cfg=cfg(sm_embed_rnnoise)");

    println!("cargo:rerun-if-changed=ui/tailwind.css");
    println!("cargo:rerun-if-changed=ui/material-symbols-outlined.woff2");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bundle_ui(&out.join("ui.html"));
    #[cfg(feature = "ui-slint")]
    slint_build::compile_with_config("ui/slint/app.slint", slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()))
        .expect("compile ui/slint/app.slint");
    // Embedded denoisers (see src/audio/plugins_bundle.rs). Both are optional: a machine without
    // speexdsp headers, or a checkout where setup-deps.sh hasn't staged RNNoise, still builds.
    if build_speex(&out.join("libspeex_ladspa.so")) {
        println!("cargo:rustc-cfg=sm_embed_speex");
    } else {
        println!("cargo:warning=speexdsp headers not found: Speex denoiser will not be bundled (install speexdsp / speexdsp-devel)");
    }
    if stage_rnnoise(Path::new("assets/plugins/librnnoise_ladspa.so"), &out.join("librnnoise_ladspa.so")) {
        println!("cargo:rustc-cfg=sm_embed_rnnoise");
    }
}

fn build_speex(dest: &Path) -> bool {
    let Ok(flags) = Command::new("pkg-config").args(["--cflags", "--libs", "speexdsp"]).output() else { return false };
    if !flags.status.success() {
        return false;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let mut cmd = Command::new(cc);
    cmd.args(["-O2", "-fPIC", "-shared", "-Wall", "-o"]).arg(dest).arg("plugins/speex-ladspa/speex_ladspa.c");
    cmd.args(String::from_utf8_lossy(&flags.stdout).split_whitespace());
    matches!(cmd.status(), Ok(s) if s.success())
}

/// Copies a staged, checksum-matching RNNoise plugin into OUT_DIR for `include_bytes!`.
fn stage_rnnoise(src: &Path, dest: &Path) -> bool {
    if !src.exists() {
        return false;
    }
    let ok = Command::new("sha256sum")
        .arg(src)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).starts_with(RNNOISE_SHA256))
        .unwrap_or(false);
    if !ok {
        println!("cargo:warning={} does not match the pinned SHA256; not embedding it", src.display());
        return false;
    }
    std::fs::copy(src, dest).is_ok()
}

/// Writes the UI with its two network resources inlined: the Tailwind CDN script (which compiled
/// CSS at runtime, so the layout jumped while it loaded and broke offline) becomes the prebuilt
/// ui/tailwind.css, and the Google Fonts link becomes the bundled Material Symbols font.
/// The source HTML keeps both lines so it still opens in a browser.
fn bundle_ui(dest: &Path) {
    const TAILWIND_TAG: &str = r#"<script src="https://www.gstatic.com/antigravity/web/dev/tailwindcss.min.js"></script>"#;
    const ICONS_TAG: &str = r#"<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Material+Symbols+Outlined:opsz,wght,FILL,GRAD@20..48,100..700,0..1,-50..200" />"#;
    let html = std::fs::read_to_string("mic-studio-mockup.html").expect("read mic-studio-mockup.html");
    let css = std::fs::read_to_string("ui/tailwind.css").expect("read ui/tailwind.css");
    let font = std::fs::read("ui/material-symbols-outlined.woff2").expect("read ui/material-symbols-outlined.woff2");
    assert!(html.contains(TAILWIND_TAG), "mic-studio-mockup.html: Tailwind CDN tag changed; update build.rs bundle_ui");
    assert!(html.contains(ICONS_TAG), "mic-studio-mockup.html: Material Symbols link changed; update build.rs bundle_ui");
    let font_css = format!(
        "@font-face {{ font-family: 'Material Symbols Outlined'; font-style: normal; font-weight: 400; font-display: block; \
         src: url(data:font/woff2;base64,{}) format('woff2'); }}",
        base64(&font)
    );
    let html = html
        .replacen(TAILWIND_TAG, &format!("<style>{css}</style>"), 1)
        .replacen(ICONS_TAG, &format!("<style>{font_css}</style>"), 1);
    std::fs::write(dest, html).expect("write ui.html");
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}
