//! Embeds the built frontend (`upstream/lrclib-homepage/dist`) into the binary and sets `LRCLIB_MIRROR_VERSION`:
//! the package version, `LRCLIB_MIRROR_VERSION_SUFFIX` (e.g. `-dev+abc1234` for CI builds) and the commit of the
//! web search. Without a built frontend the binary still builds and serves a placeholder page, so the backend
//! can be developed without node.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());

    println!("cargo::rerun-if-env-changed=LRCLIB_MIRROR_VERSION_SUFFIX");
    let suffix = std::env::var("LRCLIB_MIRROR_VERSION_SUFFIX").unwrap_or_default();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let homepage = upstream_commit(&root, "lrclib-homepage", "LRCLIB_HOMEPAGE_COMMIT");
    println!(
        "cargo::rustc-env=LRCLIB_MIRROR_VERSION={version}{suffix} (lrclib-homepage {homepage})"
    );

    let dist = root.join("upstream").join("lrclib-homepage").join("dist");
    println!("cargo::rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.is_dir() {
        collect(&dist, &dist, &mut files);
    }
    files.sort();

    let mut out = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        let _ = writeln!(
            out,
            "    ({name:?}, include_bytes!({:?})),",
            path.display().to_string()
        );
    }
    out.push_str("];\n");

    let target = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("assets.rs");
    if let Err(e) = std::fs::write(&target, out) {
        panic!("writing {}: {e}", target.display());
    }
}

/// The short commit of a submodule: from `env` when set (builds without the git metadata, like a Docker build
/// context), otherwise from git.
fn upstream_commit(root: &Path, name: &str, env: &str) -> String {
    println!("cargo::rerun-if-env-changed={env}");
    if let Ok(commit) = std::env::var(env)
        && !commit.is_empty()
    {
        return commit.chars().take(7).collect();
    }
    let dir = root.join("upstream").join(name);
    // The submodule's HEAD moves when it is updated.
    println!(
        "cargo::rerun-if-changed={}",
        root.join(".git/modules/upstream")
            .join(name)
            .join("HEAD")
            .display()
    );
    Command::new("git")
        .args([
            "-C",
            &dir.display().to_string(),
            "rev-parse",
            "--short=7",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "unknown".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )
}

fn collect(dist: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(dist, &path, files);
        } else if let Ok(name) = path.strip_prefix(dist) {
            let name = name
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push((name, path));
        }
    }
}
