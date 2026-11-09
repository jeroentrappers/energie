//! Command line: `energie --config <file>` runs; `--check-config <file>` validates.

use std::path::PathBuf;

use crate::config::Config;
use crate::drivers::Drivers;
use crate::ha::entity::parse_time;
use crate::replay::ReplayConfig;

pub const USAGE: &str = "\
energie — home energy controller

usage:
  energie --config <file.toml>         run
  energie --check-config <file.toml>   validate the config and exit
  energie import-settings --from-ha --config <file.toml> [--force]
                                       copy HA's helpers (settings, latches, counters, meters)
                                       into the state dir, read-only, once
  energie replay --history <export.csv[.gz]> [--from <time>] [--to <time>]
          [--step-ms 1000] [--report out.json] [--summary out.md] [--lockstep | --no-lockstep]
          [--trace <field>]
                                       replay an HA recorder export through the engine
                                       and compare with what HA decided
  energie credentials set <device> [--url http://127.0.0.1:8081]
                                       new credentials for a cloud device (Kia, VW): asked
                                       here, kept by the running service, which logs in at once
  energie credentials status [--url http://127.0.0.1:8081]
                                       how each cloud device's sign-in is doing
  energie --healthcheck [port]         ask the local service for /healthz (default port 8080);
                                       exit 0 when healthy, for container health checks
  energie --version | --help

Logging: RUST_LOG (default: info), to stderr.";

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Run(PathBuf),
    Check(PathBuf),
    /// `import-settings --from-ha`: the config file, and whether to overwrite.
    Import(PathBuf, bool),
    Replay(Box<ReplayConfig>),
    Credentials(crate::credentials::CredCmd),
    /// `--healthcheck [port]`.
    Health(u16),
    Version,
    Help,
}

/// `replay` options (times: RFC 3339 or `YYYY-MM-DD HH:MM:SS` Brussels time).
fn parse_replay(args: &[String]) -> Result<ReplayConfig, String> {
    let mut c = ReplayConfig::default();
    let mut it = args.iter();
    let time = |v: &str| parse_time(v).ok_or_else(|| format!("bad time: {v}"));
    while let Some(a) = it.next() {
        if a == "--lockstep" || a == "--no-lockstep" {
            c.lockstep = a == "--lockstep";
            continue;
        }
        let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
        match a.as_str() {
            "--history" => c.history = v.into(),
            "--from" => c.from = Some(time(v)?),
            "--to" => c.to = Some(time(v)?),
            "--step-ms" => {
                c.step_ms = v.parse().ok().filter(|s| *s > 0).ok_or_else(|| format!("bad --step-ms: {v}"))?
            }
            "--report" => c.report = Some(v.into()),
            "--summary" => c.summary = Some(v.into()),
            "--trace" => c.trace = Some(v.clone()),
            _ => return Err(format!("unknown replay option: {a}")),
        }
    }
    if c.history.as_os_str().is_empty() {
        return Err("replay needs --history".into());
    }
    Ok(c)
}

/// `import-settings` options: `--from-ha` (the only source), `--config <file>`, `--force`.
fn parse_import(args: &[String]) -> Result<Cmd, String> {
    let (mut config, mut from_ha, mut force) = (None, false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from-ha" => from_ha = true,
            "--force" => force = true,
            "--config" | "-c" => config = Some(PathBuf::from(it.next().ok_or("--config needs a value")?)),
            _ => return Err(format!("unknown import-settings option: {a}")),
        }
    }
    if !from_ha {
        return Err("import-settings needs --from-ha (the only source)".into());
    }
    config.map(|c| Cmd::Import(c, force)).ok_or_else(|| "import-settings needs --config <file>".into())
}

pub fn parse(args: &[String]) -> Result<Cmd, String> {
    match args {
        [cmd, rest @ ..] if cmd == "replay" => parse_replay(rest).map(|c| Cmd::Replay(Box::new(c))),
        [cmd, rest @ ..] if cmd == "import-settings" => parse_import(rest),
        [cmd, rest @ ..] if cmd == "credentials" => crate::credentials::parse(rest).map(Cmd::Credentials),
        [flag, path] if flag == "--config" || flag == "-c" => Ok(Cmd::Run(path.into())),
        [flag, path] if flag == "--check-config" => Ok(Cmd::Check(path.into())),
        [flag] if flag == "--healthcheck" => Ok(Cmd::Health(8080)),
        [flag, port] if flag == "--healthcheck" => {
            port.parse().map(Cmd::Health).map_err(|_| format!("--healthcheck: not a port: {port}"))
        }
        [flag] if flag == "--version" || flag == "-V" => Ok(Cmd::Version),
        [flag] if flag == "--help" || flag == "-h" => Ok(Cmd::Help),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
}

/// Ctrl-C (no SIGTERM outside Unix).
#[cfg(not(unix))]
async fn signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// SIGINT or SIGTERM.
#[cfg(unix)]
async fn signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// `--healthcheck`: 0 when the service on this machine answers `/healthz` with 200. A container
/// image without a shell has nothing else to run.
fn healthcheck(port: u16) -> i32 {
    use std::io::{Read, Write};
    let timeout = std::time::Duration::from_secs(3);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let status = || -> std::io::Result<bool> {
        let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head)?;
        Ok(head.starts_with(b"HTTP/1.") && &head[8..] == b" 200")
    };
    i32::from(!status().unwrap_or(false))
}

/// `energie replay`: the summary to stdout, the report files as configured.
fn replay(c: ReplayConfig) -> i32 {
    match crate::replay::run(c) {
        Ok(r) => {
            println!("{}", crate::replay::report::markdown(&r));
            0
        }
        Err(e) => {
            eprintln!("replay failed: {e}");
            1
        }
    }
}

/// `energie import-settings --from-ha`: 0 when written, 2 for a bad config, 1 otherwise.
fn import(path: &std::path::Path, force: bool) -> i32 {
    let cfg = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let now = chrono::Utc::now().with_timezone(&energie_core::time::TZ);
    match rt.block_on(crate::import::import_settings(&cfg, force, now)) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("import failed: {e}");
            1
        }
    }
}

/// Run the command line; returns the process exit code.
pub fn main(args: Vec<String>) -> i32 {
    let cmd = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return 2;
        }
    };
    let path = match cmd {
        Cmd::Health(port) => return healthcheck(port),
        Cmd::Version => {
            println!("energie {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Cmd::Help => {
            println!("{USAGE}");
            return 0;
        }
        Cmd::Check(p) => {
            return match Config::load(&p) {
                Ok(c) => {
                    for w in c.warnings() {
                        eprintln!("warning: {w}");
                    }
                    println!("{}: ok (mode {})", p.display(), c.mode.label());
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    2
                }
            };
        }
        Cmd::Replay(c) => return replay(*c),
        Cmd::Import(p, force) => return import(&p, force),
        Cmd::Credentials(c) => return crate::credentials::main(c),
        Cmd::Run(p) => p,
    };
    init_tracing();
    let cfg = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "bad config");
            return 2;
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    let run = async {
        let drivers = Drivers::new(&cfg)?;
        crate::app::run(cfg, &drivers, Box::pin(signal())).await
    };
    match rt.block_on(run) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "energie failed");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 200 is healthy; a 503, an unfinished answer or nothing listening is not.
    #[test]
    fn healthcheck_needs_a_200() {
        use std::io::{Read, Write};
        let serve = |reply: &'static [u8]| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let _ = s.read(&mut [0u8; 256]);
                let _ = s.write_all(reply);
            });
            port
        };
        assert_eq!(healthcheck(serve(b"HTTP/1.1 200 OK\r\n\r\n")), 0);
        assert_eq!(healthcheck(serve(b"HTTP/1.1 503 Service Unavailable\r\n\r\n")), 1);
        assert_eq!(healthcheck(serve(b"nope")), 1);
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(healthcheck(closed), 1);
    }

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The four command forms parse; anything else is an error.
    #[test]
    fn argument_parsing() {
        assert_eq!(parse(&a(&["--config", "x.toml"])), Ok(Cmd::Run("x.toml".into())));
        assert_eq!(parse(&a(&["-c", "x.toml"])), Ok(Cmd::Run("x.toml".into())));
        assert_eq!(parse(&a(&["--check-config", "x.toml"])), Ok(Cmd::Check("x.toml".into())));
        assert_eq!(parse(&a(&["--version"])), Ok(Cmd::Version));
        assert_eq!(parse(&a(&["--healthcheck"])), Ok(Cmd::Health(8080)));
        assert_eq!(parse(&a(&["--healthcheck", "9090"])), Ok(Cmd::Health(9090)));
        assert!(parse(&a(&["--healthcheck", "x"])).unwrap_err().contains("not a port"));
        assert_eq!(parse(&a(&["-h"])), Ok(Cmd::Help));
        assert!(parse(&a(&[])).is_err());
        assert!(parse(&a(&["--config"])).is_err());
        let imp = |v: &[&str]| parse(&[a(&["import-settings"]), a(v)].concat());
        assert_eq!(imp(&["--from-ha", "--config", "x.toml"]), Ok(Cmd::Import("x.toml".into(), false)));
        assert_eq!(imp(&["-c", "x.toml", "--force", "--from-ha"]), Ok(Cmd::Import("x.toml".into(), true)));
        assert!(imp(&["--config", "x.toml"]).unwrap_err().contains("--from-ha"));
        assert!(imp(&["--from-ha"]).unwrap_err().contains("--config"));
        assert!(imp(&["--from-ha", "--config"]).unwrap_err().contains("needs a value"));
        assert!(imp(&["--from-file"]).unwrap_err().contains("--from-file"));
    }

    /// `import-settings`: a bad config exits 2, an HA that is not there 1.
    #[test]
    fn import_exit_codes() {
        assert_eq!(main(a(&["import-settings", "--from-ha", "--config", "/nonexistent.toml"])), 2);
        let d = tempfile::tempdir().unwrap();
        let tok = d.path().join("token");
        std::fs::write(&tok, "x").unwrap();
        let cfg = d.path().join("c.toml");
        std::fs::write(
            &cfg,
            format!(
                "mode = \"control\"\nstate_dir = \"{}\"\n[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"{}\"\nimport_only = true\nstart_wait_s = 1\n",
                crate::toml_path(d.path()),
                crate::toml_path(&tok)
            ),
        )
        .unwrap();
        assert_eq!(main(a(&["import-settings", "--from-ha", "--config", cfg.to_str().unwrap()])), 1);
        // Against a fake HA on a runtime of its own: written, exit 0.
        let server = tokio::runtime::Runtime::new().unwrap();
        let (url, _) = server.block_on(crate::ha::ws::tests::fake_ha(serde_json::json!([]), |_, _| async {}));
        std::fs::write(&tok, "good-token").unwrap();
        let text = std::fs::read_to_string(&cfg).unwrap().replace("http://127.0.0.1:1", &url);
        std::fs::write(&cfg, text).unwrap();
        assert_eq!(main(a(&["import-settings", "--from-ha", "--config", cfg.to_str().unwrap()])), 0);
        assert!(d.path().join("settings.json").exists());
    }

    /// Version, help, bad arguments and config checks return the right exit codes.
    #[test]
    fn exit_codes() {
        assert_eq!(main(a(&["--version"])), 0);
        assert_eq!(main(a(&["--help"])), 0);
        assert_eq!(main(a(&["--bogus"])), 2);
        let d = tempfile::tempdir().unwrap();
        let ok = d.path().join("ok.toml");
        std::fs::write(&ok, "mode = \"simulate\"\n").unwrap();
        assert_eq!(main(a(&["--check-config", ok.to_str().unwrap()])), 0);
        // The legacy tables still pass, with a deprecation warning.
        let legacy = d.path().join("legacy.toml");
        std::fs::write(&legacy, "mode = \"simulate\"\n[sources]\ndefault = \"sim\"\n").unwrap();
        assert_eq!(main(a(&["--check-config", legacy.to_str().unwrap()])), 0);
        assert_eq!(main(a(&["--check-config", "/nonexistent.toml"])), 2);
        assert_eq!(main(a(&["--config", "/nonexistent.toml"])), 2);
        // A driver selection without its [devices] entry is a bad config.
        let no_device = d.path().join("no-device.toml");
        std::fs::write(&no_device, "mode = \"simulate\"\n[sources]\np1 = \"driver\"\n").unwrap();
        assert_eq!(main(a(&["--check-config", no_device.to_str().unwrap()])), 2);
        // A config that loads but whose drivers cannot be built (missing token file).
        let bad = d.path().join("bad.toml");
        std::fs::write(
            &bad,
            format!(
                "mode = \"simulate\"\nstate_dir = \"{}\"\nlisten = \"127.0.0.1:0\"\n[sources]\np1 = \"driver\"\n[devices.homewizard]\naddress = \"127.0.0.1\"\nserial = \"x\"\ntoken_file = \"/nonexistent/hw-token\"\n",
                crate::toml_path(d.path())
            ),
        )
        .unwrap();
        assert_eq!(main(a(&["--config", bad.to_str().unwrap()])), 1);
        // A config whose drivers build but that cannot start (the port is taken).
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = d.path().join("taken.toml");
        std::fs::write(
            &taken,
            format!(
                "mode = \"simulate\"\nstate_dir = \"{}\"\nlisten = \"{}\"\n",
                crate::toml_path(d.path()),
                busy.local_addr().unwrap()
            ),
        )
        .unwrap();
        assert_eq!(main(a(&["--config", taken.to_str().unwrap()])), 1);
    }

    /// `replay` options parse into a ReplayConfig; bad values and unknown options are errors.
    #[test]
    fn replay_arguments() {
        let replay = |r: Result<Cmd, String>| r.ok().and_then(|c| if let Cmd::Replay(c) = c { Some(*c) } else { None });
        let c = replay(parse(&a(&[
            "replay",
            "--history",
            "h.csv.gz",
            "--from",
            "2026-11-03 00:00:00",
            "--to",
            "2026-11-04T00:00:00+01:00",
            "--step-ms",
            "500",
            "--report",
            "r.json",
            "--summary",
            "r.md",
            "--trace",
            "batterij_setpoint_w",
            "--no-lockstep",
        ])))
        .unwrap();
        assert_eq!(c.history, PathBuf::from("h.csv.gz"));
        assert_eq!(c.from.unwrap().to_rfc3339(), "2026-11-03T00:00:00+01:00");
        assert_eq!(c.to.unwrap().to_rfc3339(), "2026-11-04T00:00:00+01:00");
        assert_eq!((c.step_ms, c.lockstep), (500, false));
        assert_eq!(c.report, Some(PathBuf::from("r.json")));
        assert_eq!(c.summary, Some(PathBuf::from("r.md")));
        assert_eq!(c.trace.as_deref(), Some("batterij_setpoint_w"));
        assert!(replay(parse(&a(&["replay", "--no-lockstep", "--lockstep", "--history", "h"]))).unwrap().lockstep);
        assert!(replay(parse(&a(&["--version"]))).is_none());
        assert_eq!(parse(&a(&["replay"])), Err("replay needs --history".into()));
        assert_eq!(parse(&a(&["replay", "--history"])), Err("--history needs a value".into()));
        assert_eq!(parse(&a(&["replay", "--from", "gisteren"])), Err("bad time: gisteren".into()));
        assert_eq!(parse(&a(&["replay", "--step-ms", "0"])), Err("bad --step-ms: 0".into()));
        assert_eq!(parse(&a(&["replay", "--speed", "2"])), Err("unknown replay option: --speed".into()));
    }

    /// `energie replay` exits 0 with a summary, 1 when the history cannot be read.
    #[test]
    fn replay_exit_codes() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path().join("h.csv");
        std::fs::write(&h, "sensor.p1_meter_vermogen,500,1794052800,\nsensor.p1_meter_vermogen,501,1794052802,\n")
            .unwrap();
        assert_eq!(main(a(&["replay", "--history", h.to_str().unwrap()])), 0);
        assert_eq!(main(a(&["replay", "--history", "/nonexistent.csv"])), 1);
    }
}
