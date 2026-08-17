//! End-to-end CLI smoke test.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn target_dir() -> PathBuf {
    let mut path = std::env::current_dir().unwrap();
    path.push("..");
    path.push("..");
    path.push("target");
    path.push("debug");
    path
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cordis-cli-smoke-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A minimal project (one `.so` fixture) starts and exits cleanly.
#[test]
fn cli_starts_and_exits_on_signal() {
    let dir = temp_dir("smoke");
    let plugins = dir.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let fixture = target_dir().join(if cfg!(target_os = "macos") {
        "libcordis_fixture_meta.dylib"
    } else {
        "libcordis_fixture_meta.so"
    });
    let plugin_path = plugins.join(if cfg!(target_os = "macos") {
        "meta.dylib"
    } else {
        "meta.so"
    });
    fs::copy(&fixture, &plugin_path).unwrap();
    fs::write(
        dir.join("cordis.yml"),
        "- id: '1'\n  name: cordis-meta\n  config:\n    value: 1\n",
    )
    .unwrap();

    let ready_file = dir.join("ready.signal");
    let mut child = Command::new(target_dir().join("cordis-cli"))
        .current_dir(&dir)
        .arg("-c")
        .arg(dir.join("cordis.yml"))
        .arg("--plugins-dir")
        .arg(&plugins)
        .env("CORDIS_READY_FILE", &ready_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cordis-cli");

    wait_until_ready(&mut child, &ready_file);
    // SAFETY: the child pid is positive and we send a standard signal.
    libc_kill(child.id() as i32, 2); // SIGINT
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "cordis must exit 0 on SIGINT: {:?}",
        output
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cordis started"),
        "startup log missing: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("cordis exiting"),
        "exit log missing: {stdout}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// `CORDIS_SHARED` JSON is accepted by the launcher (no crash).
#[test]
fn cli_accepts_cordis_shared_env() {
    let dir = temp_dir("shared");
    let plugins = dir.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let fixture = target_dir().join(if cfg!(target_os = "macos") {
        "libcordis_fixture_meta.dylib"
    } else {
        "libcordis_fixture_meta.so"
    });
    fs::copy(
        &fixture,
        plugins.join(if cfg!(target_os = "macos") {
            "meta.dylib"
        } else {
            "meta.so"
        }),
    )
    .unwrap();
    fs::write(
        dir.join("cordis.yml"),
        "- id: '1'\n  name: cordis-meta\n  config:\n    value: 1\n",
    )
    .unwrap();
    let ready_file = dir.join("ready.signal");
    let mut child = Command::new(target_dir().join("cordis-cli"))
        .current_dir(&dir)
        .arg("-c")
        .arg(dir.join("cordis.yml"))
        .arg("--plugins-dir")
        .arg(&plugins)
        .env("CORDIS_SHARED", r#"{"startTime": 123}"#)
        .env("CORDIS_TEST_MODE", "1")
        .env("CORDIS_READY_FILE", &ready_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    // The process waits for a signal; terminating it via SIGTERM is enough to
    // prove startup did not fail on the env payload.
    wait_until_ready(&mut child, &ready_file);
    // SAFETY: standard signal on a live child.
    libc_kill(child.id() as i32, 15); // SIGTERM
    let status = child.wait_with_output().expect("wait").status;
    assert!(status.success(), "SIGTERM must exit 0: {status:?}");
    fs::remove_dir_all(&dir).unwrap();
}

fn wait_until_ready(child: &mut std::process::Child, ready_file: &std::path::Path) {
    for _ in 0..200 {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("cordis exited early with {status:?}");
        }
        if ready_file.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("cordis did not become ready within 4s");
}

/// An invalid plugin config is a startup error with entry location.
#[test]
fn cli_fails_on_invalid_plugin_config() {
    let dir = temp_dir("invalid");
    let plugins = dir.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let fixture = target_dir().join(if cfg!(target_os = "macos") {
        "libcordis_fixture_meta.dylib"
    } else {
        "libcordis_fixture_meta.so"
    });
    fs::copy(
        &fixture,
        plugins.join(if cfg!(target_os = "macos") {
            "meta.dylib"
        } else {
            "meta.so"
        }),
    )
    .unwrap();
    fs::write(
        dir.join("cordis.yml"),
        "- id: '1'\n  name: cordis-meta\n  config:\n    value: 0\n",
    )
    .unwrap();

    let output = Command::new(target_dir().join("cordis-cli"))
        .current_dir(&dir)
        .arg("-c")
        .arg(dir.join("cordis.yml"))
        .arg("--plugins-dir")
        .arg(&plugins)
        .output()
        .expect("run");
    assert!(
        !output.status.success(),
        "invalid config must be a startup error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plugin failed to apply") && stderr.contains("#1"),
        "error must carry entry location: {stderr}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[link(name = "c")]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn libc_kill(pid: i32, sig: i32) {
    // SAFETY: declared extern with the libc signature.
    unsafe { kill(pid, sig) };
}

#[cfg(not(unix))]
fn libc_kill(_pid: i32, _sig: i32) {
    unimplemented!("signal test is unix-only")
}
