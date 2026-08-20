//! Test support: ensures fixture plugin cdylibs exist before integration
//! tests load them.
//!
//! `cargo test` compiles fixture crates into hashed test harnesses under
//! `target/<profile>/deps/` but does not emit the final
//! `libcordis_fixture_*.{so,dylib}` files that loader and CLI tests open with
//! `libloading`. This crate builds the requested fixture packages with
//! `cargo build` so those artifacts are present.

use std::path::PathBuf;
use std::process::Command;

/// Returns the workspace `target/<profile>` directory that final artifacts
/// land in. Integration tests run with the package root as cwd, so two
/// levels up is the workspace root.
pub fn artifact_dir() -> PathBuf {
    if let Some(dir) =
        std::env::var_os("CARGO_TARGET_DIR").or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR"))
    {
        let mut path = PathBuf::from(dir);
        path.push(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        });
        return path;
    }
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    path.push("..");
    path.push("..");
    path.push("target");
    path.push(if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    });
    path
}

/// The cdylib file name for a fixture package, e.g. `cordis-fixture-meta` →
/// `libcordis_fixture_meta.so` (`.dylib` on macOS, `.dll` on Windows).
fn artifact_file(package: &str) -> String {
    let stem = package.replace('-', "_");
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Builds the given fixture packages when their cdylib artifacts are missing
/// from the workspace target dir. Concurrent callers (parallel tests and
/// test binaries) are serialized by cargo's target-directory lock; the build
/// is a quick no-op once the artifact is fresh.
pub fn ensure_fixtures(packages: &[&str]) {
    let missing: Vec<&str> = packages
        .iter()
        .copied()
        .filter(|package| !artifact_dir().join(artifact_file(package)).exists())
        .collect();
    if missing.is_empty() {
        return;
    }
    if let Err(message) = build_fixtures(&missing) {
        panic!("fixture plugin build failed: {message}");
    }
}

fn build_fixtures(packages: &[&str]) -> Result<(), String> {
    let mut command = Command::new("cargo");
    command.arg("build");
    for package in packages {
        command.arg("-p").arg(package);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}
