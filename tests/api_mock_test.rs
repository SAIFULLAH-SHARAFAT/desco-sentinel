//! Exercises the HTTP client against a local fake DESCO server: backend fallback, hints,
//! retry on server errors, and meter-number resolution.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use desco_sentinel::desco_api::{ApiError, DescoClient};
use desco_sentinel::domain::{Backend, Identifier};
use tiny_http::{Response, Server};

type Handler = dyn Fn(&str, &str) -> (u16, String) + Send + Sync;

/// Starts a fake server; `handler(path, query)` returns (status, body). Returns the base URL
/// and the list of requests seen as `path?query`.
fn start_mock(handler: Box<Handler>) -> (String, Arc<Mutex<Vec<String>>>) {
    let server = Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_ip().expect("ip address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_in_thread = Arc::clone(&seen);
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            seen_in_thread.lock().expect("lock").push(url.clone());
            let (path, query) = url.split_once('?').unwrap_or((&url, ""));
            let (status, body) = handler(path, query);
            let _ = request.respond(Response::from_string(body).with_status_code(status));
        }
    });
    (format!("http://{addr}/api"), seen)
}

fn client(base: &str, retries: u32) -> DescoClient {
    DescoClient::new(base, Duration::from_secs(5), retries, false).expect("client")
}

const OK_BALANCE: &str = r#"{"code":200,"desc":"OK","data":{"accountNo":"98765421","meterNo":"667788990011","balance":250.5,"currentMonthConsumption":10.0,"readingTime":"2026-10-18"}}"#;
const NULL_DATA: &str = r#"{"code":200,"desc":"OK","data":null}"#;
const NOT_FOUND: &str = r#"{"code":16001,"desc":"The Account No. does not exist","data":null}"#;
const INFO: &str = r#"{"code":200,"desc":"OK","data":{"accountNo":"98765421","meterNo":"667788990011","customerName":"X","sanctionLoad":2}}"#;

fn account() -> Identifier {
    Identifier::parse("98765421").expect("valid account")
}

#[test]
fn unified_hit_makes_one_call() {
    let (base, seen) = start_mock(Box::new(|path, _| {
        assert!(path.starts_with("/api/unified/"), "unexpected {path}");
        (200, OK_BALANCE.to_string())
    }));
    let balance = client(&base, 0).balance(&account(), None).expect("balance");
    assert_eq!(balance.backend, Backend::Unified);
    assert_eq!(seen.lock().expect("lock").len(), 1);
}

#[test]
fn falls_back_to_tkdes_when_unified_returns_null_data() {
    let (base, seen) = start_mock(Box::new(|path, _| {
        if path.contains("/unified/") {
            (200, NULL_DATA.to_string())
        } else {
            (200, OK_BALANCE.to_string())
        }
    }));
    let balance = client(&base, 0).balance(&account(), None).expect("balance");
    assert_eq!(balance.backend, Backend::Tkdes);
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert!(seen[0].contains("/unified/") && seen[1].contains("/tkdes/"));
}

#[test]
fn backend_hint_skips_the_failing_first_call() {
    let (base, seen) = start_mock(Box::new(|path, _| {
        if path.contains("/tkdes/") {
            (200, OK_BALANCE.to_string())
        } else {
            (200, NULL_DATA.to_string())
        }
    }));
    let balance = client(&base, 0)
        .balance(&account(), Some(Backend::Tkdes))
        .expect("balance");
    assert_eq!(balance.backend, Backend::Tkdes);
    assert_eq!(
        seen.lock().expect("lock").len(),
        1,
        "no wasted unified call"
    );
}

#[test]
fn not_found_everywhere_is_a_clear_error() {
    let (base, _) = start_mock(Box::new(|_, _| (200, NOT_FOUND.to_string())));
    let err = client(&base, 0)
        .balance(&account(), None)
        .expect_err("not found");
    assert!(
        matches!(err, ApiError::NotFound { code: 16001, .. }),
        "{err:?}"
    );
}

#[test]
fn server_errors_are_retried_then_succeed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in_handler = Arc::clone(&calls);
    let (base, _) = start_mock(Box::new(move |_, _| {
        if calls_in_handler.fetch_add(1, Ordering::SeqCst) < 2 {
            (503, "busy".to_string())
        } else {
            (200, OK_BALANCE.to_string())
        }
    }));
    let balance = client(&base, 2)
        .balance(&account(), None)
        .expect("balance after retries");
    assert!((balance.balance - 250.5).abs() < 1e-9);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn persistent_server_errors_give_up() {
    let (base, seen) = start_mock(Box::new(|_, _| (500, "boom".to_string())));
    let err = client(&base, 1)
        .balance(&account(), None)
        .expect_err("fails");
    assert!(matches!(err, ApiError::Http(500)), "{err:?}");
    assert_eq!(
        seen.lock().expect("lock").len(),
        2,
        "one try plus one retry"
    );
}

#[test]
fn meter_number_uses_meterno_and_never_both_params() {
    let (base, seen) = start_mock(Box::new(|_, _| (200, OK_BALANCE.to_string())));
    let id = Identifier::parse("667788990011").expect("valid meter");
    client(&base, 0).balance(&id, None).expect("balance");
    let seen = seen.lock().expect("lock");
    assert!(
        seen[0].ends_with("getBalance?meterNo=667788990011"),
        "{}",
        seen[0]
    );
    assert!(!seen[0].contains("accountNo"));
}

#[test]
fn meter_rejected_by_get_balance_is_resolved_via_customer_info() {
    let (base, seen) = start_mock(Box::new(|path, query| {
        match (path.ends_with("getBalance"), query.contains("meterNo")) {
            (true, false) => (200, OK_BALANCE.to_string()),
            (false, _) if path.contains("/unified/") => (200, INFO.to_string()),
            (true, true) | (false, _) => (200, NULL_DATA.to_string()),
        }
    }));
    let id = Identifier::parse("667788990011").expect("valid meter");
    let balance = client(&base, 0)
        .balance(&id, None)
        .expect("resolved balance");
    assert_eq!(balance.account_no, "98765421");
    let seen = seen.lock().expect("lock");
    assert!(seen.iter().any(|u| u.contains("getCustomerInfo?meterNo=")));
    assert!(
        seen.last()
            .is_some_and(|u| u.ends_with("getBalance?accountNo=98765421"))
    );
}

#[test]
fn non_json_body_is_malformed_not_a_panic() {
    let (base, _) = start_mock(Box::new(|_, _| {
        (200, "<html>maintenance</html>".to_string())
    }));
    let err = client(&base, 0)
        .balance(&account(), None)
        .expect_err("not json");
    assert!(matches!(err, ApiError::Malformed(_)), "{err:?}");
}
