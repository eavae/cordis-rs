//! `cordis create` scaffolds a project that builds and runs.
//!
//! Unix-only: the generated app is driven to a clean exit with SIGINT.
#![cfg(unix)]

use std::fs;
use std::process::Command;
use std::time::Duration;

/// Path to the built `cordis-cli` binary; `cargo test` sets this for the
/// integration tests of the package that owns the bin target.
fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cordis-cli")
}

/// The generated project builds and its example `.so` plugin loads (visible
/// through the plugin's apply log).
#[test]
fn generated_project_builds_and_runs() {
    let dir = std::env::temp_dir().join(format!("cordis-cli-scaffold-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let output = Command::new(cli_bin())
        .arg("create")
        .arg(&dir)
        .output()
        .expect("cordis create");
    assert!(
        output.status.success(),
        "cordis create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.join("cordis.yml").exists());
    assert!(dir.join("plugins/hello/src/lib.rs").exists());

    // Build the generated project (cargo is available in the dev env). The
    // standalone project fetches and compiles its own dependency tree, so
    // this can take a while on the first run.
    let mut build = Command::new("cargo")
        .arg("build")
        .current_dir(&dir)
        // The generated project must build into its own target dir even when
        // the parent test run overrides CARGO_TARGET_DIR.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cargo build");
    for _ in 0..360 {
        if let Ok(Some(status)) = build.try_wait() {
            assert!(
                status.success(),
                "generated project must build: {}",
                String::from_utf8_lossy(&build.wait_with_output().unwrap().stderr)
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        build.try_wait().unwrap().is_some(),
        "generated project build timed out"
    );

    // Copy the built hello plugin into the project's plugins dir and start.
    let plugin_name = if cfg!(target_os = "macos") {
        "libcordis_hello.dylib"
    } else {
        "libcordis_hello.so"
    };
    let built = dir.join("target/debug").join(plugin_name);
    assert!(
        built.exists(),
        "hello plugin artifact missing: {}",
        built.display()
    );
    fs::create_dir_all(dir.join("plugins")).unwrap();
    fs::copy(&built, dir.join("plugins").join(plugin_name)).unwrap();

    let ready_file = dir.join("ready.signal");
    let app_name = format!("{}-app", dir.file_name().unwrap().to_string_lossy());
    let mut child = Command::new(dir.join("target/debug").join(&app_name))
        .current_dir(&dir)
        .env("CORDIS_READY_FILE", &ready_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run generated project");
    for _ in 0..200 {
        if ready_file.exists() {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "generated project exited early with {status:?}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        ready_file.exists(),
        "generated project did not become ready"
    );

    libc_kill(child.id() as i32, 2);
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hello from the example cordis plugin"),
        "example plugin must apply: {stdout} / {stderr}"
    );
    assert!(stdout.contains("cordis exiting"), "clean exit: {stdout}");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[link(name = "c")]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn libc_kill(pid: i32, sig: i32) {
    // SAFETY: declared extern with the libc signature.
    unsafe { kill(pid, sig) };
}
