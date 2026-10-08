//! Build stamp for the in-app updater: bakes the build time (as a Unix epoch) and the git short SHA
//! into the binary so it can tell whether a published release is newer than itself.
//!
//! `SOURCE_DATE_EPOCH` (set by CI to the commit time) makes the stamp reproducible; otherwise the
//! current time is used.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    println!("cargo:rustc-env=ASLC_BUILD_EPOCH={epoch}");

    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ASLC_GIT_SHA={sha}");

    // The release tag this binary is being built for (CI sets it). Empty for local/dev builds; the
    // updater uses it to avoid offering the very release the running binary came from.
    println!("cargo:rerun-if-env-changed=ASLC_RELEASE_TAG");
    let tag = std::env::var("ASLC_RELEASE_TAG").unwrap_or_default();
    println!("cargo:rustc-env=ASLC_RELEASE_TAG={tag}");
}
