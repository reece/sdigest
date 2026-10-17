//! Sets `SDIGEST_VERSION` for `--version` and Python's `__version__`.
//!
//! The git tag is the only source of truth for versions. Release builds are
//! stamped from the tag by CI (Cargo.toml then holds the real version, as in
//! the published crate and sdist). Unstamped builds from a git checkout
//! describe themselves relative to the latest tag, like setuptools_scm:
//! `0.2.1-dev.3+g4b551b3` is 3 commits after 0.2.0.

use std::path::Path;
use std::process::Command;

fn main() {
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let version = if pkg_version == "0.0.0" {
        watch_git();
        git_version().unwrap_or(pkg_version)
    } else {
        pkg_version
    };
    println!("cargo:rustc-env=SDIGEST_VERSION={version}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_version() -> Option<String> {
    let dirty = if git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        ""
    } else {
        ".dirty"
    };
    let Some(described) = git(&["describe", "--tags", "--long", "--match", "[0-9]*"]) else {
        // No release tags yet.
        let hash = git(&["rev-parse", "--short", "HEAD"])?;
        return Some(format!("0.0.0-dev+g{hash}{dirty}"));
    };
    // <tag>-<distance>-g<hash>; the tag itself may contain '-'.
    let mut parts = described.rsplitn(3, '-');
    let (hash, distance, tag) = (parts.next()?, parts.next()?, parts.next()?);
    if distance == "0" && dirty.is_empty() {
        return Some(tag.to_string());
    }
    Some(format!("{}-dev.{distance}+{hash}{dirty}", next_patch(tag)?))
}

/// 0.2.0 -> 0.2.1; a pre-release such as 0.3.0-rc.1 -> 0.3.0.
fn next_patch(tag: &str) -> Option<String> {
    if let Some((base, _pre)) = tag.split_once('-') {
        return Some(base.to_string());
    }
    let (major_minor, patch) = tag.rsplit_once('.')?;
    Some(format!("{major_minor}.{}", patch.parse::<u64>().ok()? + 1))
}

/// Rerun when commits, tags, or the index change. Only existing paths are
/// watched; a missing path would make cargo rerun this on every build.
fn watch_git() {
    let Some(git_dir) = git(&["rev-parse", "--git-dir"]) else {
        return;
    };
    for name in ["HEAD", "index", "refs/tags", "packed-refs"] {
        let path = Path::new(&git_dir).join(name);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        let path = Path::new(&git_dir).join(head_ref);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
