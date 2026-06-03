use std::{path::Path, process::Command};

fn main() {
    // Re-run when HEAD moves so AGAVE_GIT_COMMIT_HASH tracks checkout, even if
    // this crate's sources did not change (common on experiment branches).
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let git_head = Path::new(&manifest_dir).join("../.git/HEAD");
    if git_head.exists() {
        println!("cargo:rerun-if-changed={}", git_head.display());
    }

    if let Ok(git_output) = Command::new("git").args(["rev-parse", "HEAD"]).output() {
        if git_output.status.success() {
            if let Ok(git_commit_hash) = String::from_utf8(git_output.stdout) {
                let trimmed_hash = git_commit_hash.trim().to_string();
                println!("cargo:rustc-env=AGAVE_GIT_COMMIT_HASH={trimmed_hash}");
            }
        }
    }
}
