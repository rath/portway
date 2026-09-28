//! Stamps the build with a version that names the exact commit it came from.
//!
//! `CARGO_PKG_VERSION` alone is useless for a binary that is deployed by hand:
//! it stays the same between releases, so two hosts report the same string no
//! matter how far apart their builds are. Releases are tagged `v<version>`, and
//! `git describe` measures the distance from the latest one:
//!
//! ```text
//! 0.1.0                  the tagged release itself
//! 0.1.0+5.g1a2b3c4       five commits after it, at 1a2b3c4
//! 0.1.0+5.g1a2b3c4.dirty the same, with uncommitted changes
//! ```
//!
//! Everything after `+` is SemVer build metadata, so a release compares equal
//! to its own tag. Without a tag in reach (a shallow clone) the hash alone
//! follows the manifest's version; without git (a source tarball) the
//! manifest's version is all there is.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `v0.1.0-5-g1a2b3c4` as the release, the distance, and the hash.
fn describe() -> Option<(String, u32, String)> {
    let text = git(&[
        "describe",
        "--tags",
        "--long",
        "--abbrev=7",
        "--match",
        "v[0-9]*",
    ])?;
    let mut parts = text.rsplitn(3, '-');
    let hash = parts.next()?.to_string();
    let distance = parts.next()?.parse().ok()?;
    let release = parts.next()?.strip_prefix('v')?.to_string();
    Some((release, distance, hash))
}

fn main() {
    let manifest = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());

    let mut metadata = Vec::new();
    let release = match describe() {
        Some((release, distance, hash)) => {
            if distance > 0 {
                metadata.push(distance.to_string());
                metadata.push(hash);
            } else if release != manifest {
                println!(
                    "cargo:warning=tag v{release} is on a commit whose Cargo.toml says {manifest}"
                );
            }
            release
        }
        None => {
            if let Some(hash) = git(&["rev-parse", "--short=7", "HEAD"]) {
                metadata.push(format!("g{hash}"));
            }
            manifest
        }
    };
    if dirty {
        metadata.push("dirty".into());
    }
    let version = if metadata.is_empty() {
        release
    } else {
        format!("{release}+{}", metadata.join("."))
    };

    println!("cargo:rustc-env=PORTWAY_VERSION={version}");
    // A new commit or a new tag must re-run this, or the stamped version goes
    // stale. A path that does not exist would re-run it on every build, so
    // only the ones this checkout has are watched.
    for path in [
        "../../.git/HEAD",
        "../../.git/refs/heads",
        "../../.git/refs/tags",
        "../../.git/packed-refs",
    ] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-env-changed=PORTWAY_VERSION");
}
