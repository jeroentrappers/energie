//! The `energie` binary end to end: exit codes, and a simulate run that serves `/healthz`,
//! stops cleanly on SIGTERM and leaves its state on disk.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn energie() -> Command {
    Command::new(env!("CARGO_BIN_EXE_energie"))
}

/// `--version` prints the version; unknown arguments exit 2 with the usage.
#[test]
fn version_and_bad_arguments() {
    let out = energie().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("energie "));
    let out = energie().arg("--frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    let out = energie().arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("--check-config"));
}

/// A config that cannot be loaded exits 2; one that loads but cannot start exits 1.
#[test]
fn run_failures_exit_non_zero() {
    let out = energie().arg("--config").arg("/nonexistent/energie.toml").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("driver.toml");
    std::fs::write(
        &cfg,
        format!(
            "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n[sources]\np1 = \"driver\"\n[devices.homewizard]\naddress = \"127.0.0.1\"\nserial = \"x\"\ntoken_file = \"/nonexistent/hw-token\"\n",
            dir.path().display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let out = energie().arg("--config").arg(&cfg).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/hw-token"));
}

/// `--check-config` accepts the shipped example and rejects a broken file.
#[test]
fn check_config_validates_the_example() {
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/energie.example.toml");
    let out = energie().arg("--check-config").arg(example).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok (mode shadow)"));
    // The standalone example: no HA, the freezer is the one section nobody reads.
    let standalone = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/energie.standalone.example.toml");
    let out = energie().arg("--check-config").arg(standalone).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok (mode control)"));
    let warnings = String::from_utf8_lossy(&out.stderr);
    assert_eq!(warnings.lines().filter(|l| l.starts_with("warning:")).count(), 1, "{warnings}");
    assert!(warnings.contains("no device reads load:vriezer"), "{warnings}");
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "mode = \"turbo\"\n").unwrap();
    let out = energie().arg("--check-config").arg(&bad).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// A simulate run: starts, answers `/healthz`, and on SIGTERM writes its state and exits 0.
#[cfg(unix)]
#[test]
fn simulate_run_stops_cleanly_on_sigterm() {
    simulate_run_stops_cleanly_on("-TERM");
}

/// Ctrl-C (SIGINT) stops it the same way.
#[cfg(unix)]
#[test]
fn simulate_run_stops_cleanly_on_sigint() {
    simulate_run_stops_cleanly_on("-INT");
}

#[cfg(unix)]
fn simulate_run_stops_cleanly_on(signal: &str) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("energie.toml");
    std::fs::write(
        &cfg,
        format!(
            "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 200\n[sim]\nspeed = 5.0\n",
            dir.path().join("state").display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let mut child = energie().arg("--config").arg(&cfg).stderr(Stdio::piped()).spawn().unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let mut addr = None;
    let mut lines = stderr.lines();
    let deadline = Instant::now() + Duration::from_secs(20);
    while addr.is_none() && Instant::now() < deadline {
        let line = lines.next().unwrap().unwrap();
        if let Some(i) = line.find("listening on http://") {
            let rest = &line[i + 20..];
            addr = Some(rest.split(|c: char| c.is_whitespace() || c == '\x1b').next().unwrap().to_string());
        }
    }
    let addr = addr.expect("the binary announces its address");
    let mut ok = false;
    while !ok && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        let mut s = TcpStream::connect(&addr).unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        ok = resp.starts_with("HTTP/1.1 200");
    }
    assert!(ok, "healthz turned 200");
    let kill = Command::new("kill").arg(signal).arg(child.id().to_string()).status().unwrap();
    assert!(kill.success());
    std::thread::spawn(move || for _ in lines {});
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(dir.path().join("state/settings.json").exists());
    assert!(dir.path().join("state/memory.json").exists());
}
