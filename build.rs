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

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
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
