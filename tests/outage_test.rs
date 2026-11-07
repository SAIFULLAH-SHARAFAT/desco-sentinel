//! A whole outage cycle against fake DESCO and Telegram servers: nothing can be read, the
//! recipients are told once, repeat runs stay quiet, and one message follows when readings
//! return.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use desco_sentinel::config::{Env, Settings, parse_meters};
use desco_sentinel::desco_api::DescoClient;
use desco_sentinel::domain::Bdt;
use desco_sentinel::monitor::Monitor;
use desco_sentinel::notifier::Notifier;
use desco_sentinel::state::alert_state_path;
use serde_json::Value;
use tiny_http::{Response, Server};

const OK_BALANCE: &str = r#"{"code":200,"desc":"OK","data":{"accountNo":"98765421","meterNo":"667788990011","balance":250.5,"currentMonthConsumption":10.0,"readingTime":"2026-11-07"}}"#;
const SIGN_IN: &str =
    r#"{"errorCode":"AUTHENTICATION_REQUIRED","message":"Please sign in to continue."}"#;

/// Fake DESCO: 401 while `signed_in` is false, a healthy balance afterwards.
fn start_desco(signed_in: Arc<AtomicBool>) -> String {
    let server = Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_ip().expect("ip");
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let response = if signed_in.load(Ordering::SeqCst) {
                Response::from_string(OK_BALANCE).with_status_code(200)
            } else {
                Response::from_string(SIGN_IN).with_status_code(401)
            };
            let _ = request.respond(response);
        }
    });
    format!("http://{addr}/api")
}

/// Messages the fake Telegram received, as (chat_id, text).
type Sent = Arc<Mutex<Vec<(String, String)>>>;

/// Fake Telegram: records every message.
fn start_telegram() -> (String, Sent) {
    let server = Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_ip().expect("ip");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            log.lock().expect("lock").push((
                body["chat_id"].as_str().unwrap_or_default().to_string(),
                body["text"].as_str().unwrap_or_default().to_string(),
            ));
            let _ = request.respond(Response::from_string(r#"{"ok":true}"#));
        }
    });
    (format!("http://{addr}"), seen)
}

struct Fixture {
    monitor: Monitor,
    notifier: Notifier,
    state_dir: PathBuf,
}

fn fixture(desco_base: &str, telegram_base: &str, name: &str) -> Fixture {
    let state_dir =
        std::env::temp_dir().join(format!("desco-outage-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_dir);
    let dir = state_dir.to_string_lossy().to_string();
    let env = Env::from_pairs([
        ("STATE_DIR", dir.as_str()),
        ("RATE_LIMIT_MS", "0"),
        ("NOTIFY_PROVIDER", "telegram"),
        (
            "TELEGRAM_BOT_TOKEN",
            "123456789:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ),
        ("TELEGRAM_CHAT_ID", "111,222"),
        ("TELEGRAM_API_BASE", telegram_base),
    ]);
    let settings = Settings::from_env(&env).expect("settings");
    let meters = parse_meters(
        r#"[{"unit_name": "Apt 1A", "account_no": "98765421"},
            {"unit_name": "Apt 1B", "account_no": "98765422"}]"#,
        Bdt(150.0),
    )
    .expect("meters");
    let api = DescoClient::new(desco_base, Duration::from_secs(5), 0, false).expect("client");
    Fixture {
        notifier: Notifier::from_env(&env, &settings).expect("notifier"),
        monitor: Monitor::new(api, meters, settings),
        state_dir,
    }
}

#[test]
fn outage_is_announced_once_and_recovery_once() {
    let signed_in = Arc::new(AtomicBool::new(false));
    let desco = start_desco(Arc::clone(&signed_in));
    let (telegram, sent) = start_telegram();
    let f = fixture(&desco, &telegram, "cycle");
    let texts = || sent.lock().expect("lock").clone();

    // Dry run: reported, printed, but nothing sent and nothing remembered.
    let dry = f.monitor.run_check(&f.notifier, true);
    assert!(dry.outage);
    assert_eq!((dry.checked, dry.unreadable), (2, 2));
    assert_eq!(dry.messages.len(), 1);
    assert!(texts().is_empty());
    assert!(!alert_state_path(&f.state_dir).exists());

    // First real run: both recipients get one notice that names the reason.
    let first = f.monitor.run_check(&f.notifier, false);
    assert!(first.outage);
    assert_eq!(first.notify_failures, 0);
    let sent_now = texts();
    assert_eq!(sent_now.len(), 2, "one message per recipient: {sent_now:?}");
    assert_eq!(sent_now[0].0, "111");
    assert_eq!(sent_now[1].0, "222");
    assert!(sent_now[0].1.contains("cannot read the meters"));
    assert!(sent_now[0].1.contains("sign-in") || sent_now[0].1.contains("sign in"));

    // Still down: no repeat.
    f.monitor.run_check(&f.notifier, false);
    f.monitor.run_check(&f.notifier, false);
    assert_eq!(
        texts().len(),
        2,
        "no repeated notice while the outage continues"
    );

    // Readings return: exactly one recovery message per recipient.
    signed_in.store(true, Ordering::SeqCst);
    let back = f.monitor.run_check(&f.notifier, false);
    assert!(!back.outage);
    let after = texts();
    assert_eq!(after.len(), 4);
    assert!(after[2].1.contains("Meter readings are back"));

    // Healthy and quiet afterwards.
    f.monitor.run_check(&f.notifier, false);
    assert_eq!(texts().len(), 4);

    // A new outage is announced again.
    signed_in.store(false, Ordering::SeqCst);
    f.monitor.run_check(&f.notifier, false);
    assert_eq!(texts().len(), 6);
    let _ = std::fs::remove_dir_all(&f.state_dir);
}

#[test]
fn failed_delivery_of_the_notice_is_retried() {
    let signed_in = Arc::new(AtomicBool::new(false));
    let desco = start_desco(signed_in);
    // Nothing listens on this port, so every Telegram call fails.
    let f = fixture(&desco, "http://127.0.0.1:9", "retry");
    let first = f.monitor.run_check(&f.notifier, false);
    assert!(first.outage);
    assert_eq!(first.notify_failures, 1);

    // Same state on disk: still not remembered, so the next run tries again.
    let second = f.monitor.run_check(&f.notifier, false);
    assert_eq!(second.notify_failures, 1);
    let _ = std::fs::remove_dir_all(&f.state_dir);
}
