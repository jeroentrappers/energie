use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Path;
use axum::routing::{get, post};
use serde_json::json;

use super::*;

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

#[test]
fn parses_set_and_status() {
    assert_eq!(parse(&args("set ev3")), Ok(CredCmd::Set { device: "ev3".into(), url: DEFAULT_URL.into() }));
    assert_eq!(
        parse(&args("set --url http://127.0.0.1:9000 buzz")),
        Ok(CredCmd::Set { device: "buzz".into(), url: "http://127.0.0.1:9000".into() })
    );
    assert_eq!(parse(&args("status")), Ok(CredCmd::Status { url: DEFAULT_URL.into() }));
    for bad in ["", "set", "set a b", "status x", "status --url", "nope", "set --bogus"] {
        assert!(parse(&args(bad)).is_err(), "{bad}");
    }
    assert_eq!(
        crate::cli::parse(&args("credentials status --url http://h:1")),
        Ok(crate::cli::Cmd::Credentials(CredCmd::Status { url: "http://h:1".into() }))
    );
    assert_eq!(crate::cli::main(args("credentials set")), 2);
    assert_eq!(fields_for("kia_connect").unwrap()[1], ("password", "Password", true, true));
    assert_eq!(fields_for("cupra_data_act").unwrap().len(), 2);
    assert_eq!(fields_for("lg_thinq"), None);
}

/// A stand-in for the running service: `/api/cloud` and the credentials endpoint, which
/// records what it was sent.
async fn service(answer: Value, code: u16) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let rec = seen.clone();
    let app = Router::new()
        .route(
            "/api/cloud",
            get(|| async {
                axum::Json(json!({
                    "ev3": {"kind": "kia_connect", "auth": {"state": "aanmelden nodig", "credentials": "etc",
                        "last_login": "2026-11-08T12:00:00Z", "last_result": "unauthorized: bad", "rejected_logins": 2,
                        "next_login": "2026-11-08T12:30:00Z"}},
                    "buzz": {"kind": "cupra_data_act", "auth": {"state": "ok", "credentials": "state"}},
                    "odd": {"kind": "lg_thinq", "auth": {}}
                }))
            }),
        )
        .route(
            "/api/cloud/{id}/credentials",
            post(move |Path(id): Path<String>, axum::Json(body): axum::Json<Value>| {
                let (rec, answer) = (rec.clone(), answer.clone());
                async move {
                    rec.lock().unwrap().push((id, body));
                    (axum::http::StatusCode::from_u16(code).unwrap(), axum::Json(answer))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// `set` asks the kind's fields, leaves out an empty optional one, posts them and reports
/// the sign-in state; `status` lists every device.
#[tokio::test]
async fn set_and_status_against_the_service() {
    let ok = json!({"device": "ev3", "auth": {"state": "ok", "credentials": "state", "last_login": "2026-11-08T12:00:00Z", "last_result": "ok"}});
    let (url, seen) = service(ok, 200).await;
    let mut answers = Lines("user@example.invalid\nhunter2\n\n".as_bytes());
    let (text, good) = set(&url, "ev3", &mut answers).await.unwrap();
    assert!(good);
    assert_eq!(text, "ev3 (kia_connect): ok, credentials from state, last login 2026-11-08T12:00:00Z: ok");
    let posted = seen.lock().unwrap().clone();
    assert_eq!(posted, vec![("ev3".to_string(), json!({"username": "user@example.invalid", "password": "hunter2"}))]);

    let s = status(&url).await.unwrap();
    assert!(s.contains("buzz (cupra_data_act): ok, credentials from state"), "{s}");
    assert!(s.contains("ev3 (kia_connect): aanmelden nodig, credentials from etc, last login 2026-11-08T12:00:00Z: unauthorized: bad, 2 rejected in a row, next login 2026-11-08T12:30:00Z"), "{s}");

    let e = set(&url, "nope", &mut Lines("".as_bytes())).await.unwrap_err();
    assert!(e.contains("does not sign in") && e.contains("ev3"), "{e}");
    let e = set(&url, "odd", &mut Lines("".as_bytes())).await.unwrap_err();
    assert!(e.contains("no credentials prompt"), "{e}");
    let e = set(&url, "ev3", &mut Lines("\n".as_bytes())).await.unwrap_err();
    assert_eq!(e, "username is required");
}

/// A failed login is a non-zero outcome; refusals and an unreachable service are errors.
#[tokio::test]
async fn failures() {
    let (url, _) = service(json!({"auth": {"state": "geblokkeerd", "credentials": "state"}}), 200).await;
    let (text, good) = set(&url, "buzz", &mut Lines("e\np\n".as_bytes())).await.unwrap();
    assert!(!good && text.starts_with("buzz (cupra_data_act): geblokkeerd"), "{text}");
    let (url, _) = service(json!("alleen vanaf deze machine"), 403).await;
    let e = set(&url, "buzz", &mut Lines("e\np\n".as_bytes())).await.unwrap_err();
    assert!(e.contains("HTTP 403"), "{e}");
    let (url, _) = service(json!(null), 200).await;
    assert!(set(&url, "buzz", &mut Lines("e\np\n".as_bytes())).await.unwrap_err().contains("not a JSON answer"));
    let dead = "http://127.0.0.1:9";
    assert!(status(dead).await.unwrap_err().contains("cannot reach"));
}

/// The command's exit codes, against a service that is not there, an empty one and a good one.
#[test]
fn main_exit_codes() {
    assert_eq!(main(CredCmd::Status { url: "http://127.0.0.1:9".into() }), 1);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let empty = rt.block_on(async {
        let app = Router::new().route("/api/cloud", get(|| async { axum::Json(json!({})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    });
    assert_eq!(rt.block_on(status(&empty)).unwrap(), "no device signs in with credentials");
    assert_eq!(main(CredCmd::Status { url: empty.clone() }), 0);
    // Not a terminal: the answers come from stdin, which is empty here.
    assert_eq!(main(CredCmd::Set { device: "ev3".into(), url: empty }), 1);
    let bad = rt.block_on(async {
        let app = Router::new()
            .route("/api/cloud", get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "x") }))
            .route("/x/api/cloud", get(|| async { "[1]" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    });
    assert!(rt.block_on(status(&bad)).unwrap_err().contains("HTTP 500"));
    assert!(rt.block_on(status(&format!("{bad}/x"))).unwrap_err().contains("not a JSON object"));
}

/// The terminal prompt, in a child without a controlling terminal (`setsid`): a secret needs
/// one and fails; a plain answer comes from stdin (empty here). The `credentials` command is
/// dispatched from the command line.
#[cfg(target_os = "linux")] // setsid
#[test]
fn terminal_prompt_needs_a_terminal() {
    if std::env::var_os("ENERGIE_PROMPT_CHILD").is_some() {
        assert!(Terminal.ask("Password", true).is_err(), "no /dev/tty in a new session");
        assert_eq!(Terminal.ask("Username", false).unwrap(), "");
        assert_eq!(crate::cli::main(args("credentials status --url http://127.0.0.1:9")), 1);
        return;
    }
    let out = std::process::Command::new("setsid")
        .arg("--wait")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "credentials::tests::terminal_prompt_needs_a_terminal", "--nocapture"])
        .env("ENERGIE_PROMPT_CHILD", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
}
