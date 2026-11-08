//! Denoiser plugins embedded in the binary (see build.rs), written to `~/.local/lib/ladspa/` on
//! startup when the system doesn't already have them. Running the bare executable is then enough
//! for Speex and RNNoise.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// (file name, bytes, owned): `owned` plugins are built by Studio Mic itself, so a copy in the
/// user dir that differs from the embedded one is ours and gets updated. Third-party ones are
/// only ever written when missing.
fn embedded() -> Vec<(&'static str, &'static [u8], bool)> {
    #[allow(unused_mut)]
    let mut v: Vec<(&'static str, &'static [u8], bool)> = Vec::new();
    #[cfg(sm_embed_speex)]
    v.push(("libspeex_ladspa.so", include_bytes!(concat!(env!("OUT_DIR"), "/libspeex_ladspa.so")), true));
    #[cfg(sm_embed_rnnoise)]
    v.push(("librnnoise_ladspa.so", include_bytes!(concat!(env!("OUT_DIR"), "/librnnoise_ladspa.so")), false));
    v
}

pub fn embedded_names() -> Vec<&'static str> {
    embedded().into_iter().map(|(n, _, _)| n).collect()
}

pub fn user_ladspa_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local").join("lib").join("ladspa")
}

/// Extracts every embedded plugin that `search` doesn't already provide. Returns what was written.
pub fn ensure_embedded_plugins(search: &[String]) -> Vec<PathBuf> {
    ensure_in(&user_ladspa_dir(), search, &embedded())
}

fn ensure_in(dir: &Path, search: &[String], plugins: &[(&str, &[u8], bool)]) -> Vec<PathBuf> {
    let mut written = Vec::new();
    for (name, bytes, owned) in plugins {
        let target = dir.join(name);
        let existing = search.iter().map(|d| Path::new(d).join(name)).find(|p| p.exists());
        let needs_write = match &existing {
            None => true,
            // Refresh our own build in the user dir if it's stale; leave every other copy alone.
            Some(p) => *owned && *p == target && fs::read(p).map(|b| b != *bytes).unwrap_or(true),
        };
        if !needs_write {
            continue;
        }
        if fs::create_dir_all(dir).is_err() {
            continue;
        }
        // Atomic replace: a running filter-chain keeps the old file mapped, new ones load the new.
        let tmp = dir.join(format!(".{}.tmp", name));
        if fs::write(&tmp, bytes).is_ok()
            && fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).is_ok()
            && fs::rename(&tmp, &target).is_ok()
        {
            println!("Installed bundled LADSPA plugin {}", target.display());
            written.push(target);
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sm-bundle-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn extracts_missing_and_keeps_system_copies() {
        let user = tmpdir("user");
        let sys = tmpdir("sys");
        fs::create_dir_all(&sys).unwrap();
        fs::write(sys.join("libthird.so"), b"system").unwrap();
        let search = vec![user.to_string_lossy().into_owned(), sys.to_string_lossy().into_owned()];
        let plugins: Vec<(&str, &[u8], bool)> = vec![("libours.so", b"v1", true), ("libthird.so", b"bundled", false)];

        let w = ensure_in(&user, &search, &plugins);
        assert_eq!(w, vec![user.join("libours.so")]);
        assert_eq!(fs::read(user.join("libours.so")).unwrap(), b"v1");
        assert_eq!(fs::metadata(user.join("libours.so")).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!user.join("libthird.so").exists(), "system copy must win");

        // Second run is a no-op; a newer build of our own plugin replaces the stale copy.
        assert!(ensure_in(&user, &search, &plugins).is_empty());
        let plugins2: Vec<(&str, &[u8], bool)> = vec![("libours.so", b"v2", true)];
        assert_eq!(ensure_in(&user, &search, &plugins2).len(), 1);
        assert_eq!(fs::read(user.join("libours.so")).unwrap(), b"v2");

        let _ = fs::remove_dir_all(&user);
        let _ = fs::remove_dir_all(&sys);
    }
}
