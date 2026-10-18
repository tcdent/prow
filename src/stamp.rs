//! Build identity, for a crate's `build.rs`:
//!
//! ```ignore
//! // build.rs, with `prow` in [build-dependencies]
//! fn main() { prow::stamp::emit(); }
//! ```
//!
//! sets `PROW_COMMIT` (the short commit, or `unknown` outside git), `PROW_DIRTY` (`1` when
//! tracked files differ from it) and `PROW_FEATURES` (the crate's enabled features, comma
//! separated: the categories compiled in, among others) for the crate's compilation, read back
//! by [`build_attrs!`](crate::build_attrs). The build script reruns when the crate's files or
//! git's HEAD change.

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn emit() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(&dir).args(args).output().ok().filter(|o| o.status.success());

    let commit = git(&["rev-parse", "--short=12", "HEAD"])
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    // `diff --quiet` exits 1 when tracked files differ from HEAD
    let dirty = Command::new("git").arg("-C").arg(&dir).args(["diff", "--quiet", "HEAD", "--"]).status().map(|s| s.code() == Some(1)).unwrap_or(false);
    let features = std::env::var("CARGO_CFG_FEATURE").unwrap_or_default();

    println!("cargo:rustc-env=PROW_COMMIT={commit}");
    println!("cargo:rustc-env=PROW_DIRTY={}", dirty as u8);
    println!("cargo:rustc-env=PROW_FEATURES={features}");

    // rerun on a new commit (HEAD, or the branch it names) and on any change to the crate's files
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())) {
        rerun(&git_dir.join("HEAD"));
        rerun(&git_dir.join("index"));
        if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
            if let Some(r) = head.strip_prefix("ref: ") {
                rerun(&git_dir.join(r.trim()));
            }
        }
    }
    rerun(&dir);
}

fn rerun(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
