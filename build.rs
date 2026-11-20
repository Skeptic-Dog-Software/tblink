//! Embed the git commit in the version, so `tblink doctor` can tell two
//! builds of the same release apart.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if Path::new(".git").exists() {
        for p in [".git/HEAD", ".git/refs/heads", ".git/index"] {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    let mut v = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_default();
    if !v.is_empty() && git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty()) {
        v.push_str("-dirty");
    }
    println!("cargo:rustc-env=TBLINK_GIT={v}");
}
