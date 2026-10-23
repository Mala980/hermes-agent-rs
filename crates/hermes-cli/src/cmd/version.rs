//! `hermes version` — build and install information.
//!
//! Rust analog of the original's `_startup_fast.print_fast_version_info`
//! (details from `hermes_cli/version_info.py`): the canonical version line,
//! install directory, install method, target triple, build profile, and the
//! GitHub release update check. Every block degrades gracefully — a failed
//! probe can never take the basic version output down.

use anyhow::Result;
use std::path::Path;

#[derive(clap::Args, Debug, Clone)]
pub struct VersionArgs {
    /// Skip the update check.
    #[arg(long)]
    pub no_update_check: bool,
}

/// Classify how this binary was installed from its executable path.
///
/// Mirrors the original's `detect_install_method` intent for a Rust binary:
/// `cargo install` drops binaries under `~/.cargo/bin`, nix builds live
/// under `/nix/store`, and anything under a cargo `target/` dir is a source
/// build.
pub fn detect_install_method(exe_path: &Path) -> &'static str {
    let path = exe_path.to_string_lossy().to_lowercase();
    // Normalize separators so Windows paths match too.
    let path = path.replace('\\', "/");
    if path.contains("/.cargo/bin/") || path.ends_with("/.cargo/bin") {
        "cargo install"
    } else if path.contains("/nix/store/") {
        "nix"
    } else if path.contains("/target/debug/") {
        "source build (cargo, debug)"
    } else if path.contains("/target/release/") {
        "source build (cargo, release)"
    } else {
        "unknown"
    }
}

fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Handle `hermes version`.
pub async fn run(args: &VersionArgs) -> Result<()> {
    println!("Hermes Agent v{}", env!("CARGO_PKG_VERSION"));

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            println!("Install directory: {}", dir.display());
        }
        println!("Install method: {}", detect_install_method(&exe));
    }

    println!(
        "Target: {}-{} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH,
        build_profile()
    );

    if !args.no_update_check {
        // Synchronous update status; any failure prints nothing (the check
        // has its own timeouts, and a broken network must not break
        // `hermes version`).
        if let Ok(report) = crate::update::check_for_updates().await {
            println!();
            println!("{report}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_install_method_classifies_layouts() {
        assert_eq!(
            detect_install_method(Path::new("/home/u/.cargo/bin/hermes")),
            "cargo install"
        );
        assert_eq!(
            detect_install_method(Path::new("/nix/store/abc123-hermes-0.2.0/bin/hermes")),
            "nix"
        );
        assert_eq!(
            detect_install_method(Path::new("/repo/target/debug/hermes")),
            "source build (cargo, debug)"
        );
        assert_eq!(
            detect_install_method(Path::new("/repo/target/release/hermes")),
            "source build (cargo, release)"
        );
        assert_eq!(
            detect_install_method(Path::new("/usr/local/bin/hermes")),
            "unknown"
        );
        // Windows separators.
        assert_eq!(
            detect_install_method(Path::new(r"C:\Users\u\.cargo\bin\hermes.exe")),
            "cargo install"
        );
    }

    #[test]
    fn version_is_semver_like() {
        let v = env!("CARGO_PKG_VERSION");
        assert!(!v.is_empty());
        assert!(
            v.split('.').count() >= 2,
            "expected a dotted version, got {v:?}"
        );
    }

    #[test]
    fn build_profile_is_known() {
        assert!(matches!(build_profile(), "debug" | "release"));
    }
}
