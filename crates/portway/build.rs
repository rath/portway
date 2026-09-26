//! Stamps the build with a version that moves on every commit.
//!
//! `CARGO_PKG_VERSION` alone is useless for a binary that is deployed by hand:
//! it stays `0.1.0` until someone edits the manifest, so the receiver host and
//! the sender host both report the same string no matter how far apart their
//! builds are. Git is the only thing that moves here, so the commit count and
//! the short hash become the version.
//!
//! The number is monotonic on the default branch (a rewritten history keeps its
//! count), which makes it usable for "is the receiver older than the sender?".

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

fn main() {
    // `--always` keeps this working in a shallow clone and a source tarball,
    // where the count is missing but a hash usually is.
    let count = git(&["rev-list", "--count", "HEAD"]);
    let hash = git(&["rev-parse", "--short=7", "HEAD"]);
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());

    let base = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let version = match (count, hash) {
        (Some(count), Some(hash)) => {
            // Keep the manifest's major.minor (the human-facing release line)
            // and let the commit count supply the patch: 0.1.<commits>.
            let mut parts = base.split('.');
            let major = parts.next().unwrap_or("0");
            let minor = parts.next().unwrap_or("0");
            let mut v = format!("{major}.{minor}.{count}+{hash}");
            if dirty {
                v.push_str(".dirty");
            }
            v
        }
        _ => base,
    };

    println!("cargo:rustc-env=PORTWAY_VERSION={version}");
    // A new commit must re-run this, or the stamped version goes stale.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-env-changed=PORTWAY_VERSION");
}