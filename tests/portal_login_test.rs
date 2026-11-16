//! The signed-in portal flow against a fake portal: XSRF handshake, session cookie,
//! account id lookup, automatic re-login after the session expires, and failure reporting.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use desco_sentinel::desco_api::{ApiError, Credentials, DescoClient, Realtime};
use desco_sentinel::domain::Identifier;
use tiny_http::{Header, Method, Response, Server};

const ACCOUNTS: &str = r#"{"accounts":[
  {"accountId":41810,"sourceSystem":"UNIFIED","accountNo":"41167724","meterNo":"030119013826","status":"ACTIVE","linkStatus":"ACTIVE"},
  {"accountId":41999,"sourceSystem":"UNIFIED","accountNo":"11111111","meterNo":"030100000001","status":"ACTIVE","linkStatus":"REMOVED"}
]}"#;
const BALANCE: &str = r#"{"accountNo":"41167724","meterNo":"030119013826","balance":362.91,"meterType":"UNIFIED","currentMonthConsumedTaka":370.83,"readingTime":"2026-11-15"}"#;

#[derive(Default)]
struct Portal {
    logins: AtomicUsize,
    /// Session id currently accepted. A new login replaces it.
    valid_session: Mutex<String>,
    /// Status shown for the meter in the realtime summary (`AVAILABLE`, `DAILY_LIMIT_REACHED`).
    realtime_status: Mutex<String>,
    realtime_posts: AtomicUsize,
    realtime_polls: AtomicUsize,
}

impl Portal {
    fn with_realtime_status(status: &str) -> Arc<Self> {
        let portal = Self::default();
        *portal.realtime_status.lock().expect("lock") = status.to_string();
        Arc::new(portal)
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("header")
}

fn cookie_value(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))?
        .value
        .as_str()
        .split(';')
        .find_map(|pair| {
            let (n, v) = pair.trim().split_once('=')?;
            (n == name).then(|| v.to_string())
        })
}

/// Starts the fake portal. `password` is the only one it accepts. Returns the base URL.
fn start_portal(password: &'static str, state: Arc<Portal>) -> String {
    let server = Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_ip().expect("ip address");
    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("").to_string();
            let method = request.method().clone();
            let session = cookie_value(&request, "DESCO_SESSION");
            let xsrf_cookie = cookie_value(&request, "XSRF-TOKEN");
            let xsrf_header = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("X-XSRF-TOKEN"))
                .map(|h| h.value.as_str().to_string());

            let response = match (method, path.as_str()) {
                (Method::Get, "/api/auth/csrf") => Response::from_string("")
                    .with_status_code(204)
                    .with_header(header("Set-Cookie", "XSRF-TOKEN=xsrf-abc; Path=/")),
                (Method::Post, "/api/auth/login") => {
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).expect("body");
                    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
                    let csrf_ok = xsrf_header.is_some() && xsrf_header == xsrf_cookie;
                    let creds_ok =
                        json["username"] == "01700000000" && json["password"] == password;
                    if !csrf_ok {
                        Response::from_string("{}").with_status_code(403)
                    } else if !creds_ok {
                        Response::from_string(r#"{"errorCode":"INVALID_CREDENTIALS"}"#)
                            .with_status_code(401)
                    } else {
                        let n = state.logins.fetch_add(1, Ordering::SeqCst) + 1;
                        let id = format!("session-{n}");
                        state.valid_session.lock().expect("lock").clone_from(&id);
                        Response::from_string(
                            r#"{"authenticated":true,"mustChangePassword":false}"#,
                        )
                        .with_header(header(
                            "Set-Cookie",
                            &format!("DESCO_SESSION={id}; Path=/; HttpOnly"),
                        ))
                    }
                }
                (Method::Get, p) if p.starts_with("/api/accounts") => {
                    let valid = state.valid_session.lock().expect("lock").clone();
                    if session.as_deref() != Some(valid.as_str()) || valid.is_empty() {
                        Response::from_string(r#"{"errorCode":"AUTHENTICATION_REQUIRED"}"#)
                            .with_status_code(401)
                    } else if p == "/api/accounts" {
                        Response::from_string(ACCOUNTS)
                    } else if p == "/api/accounts/41810/balance" {
                        Response::from_string(BALANCE)
                    } else if p == "/api/accounts/realtime-balance" {
                        let status = state.realtime_status.lock().expect("lock").clone();
                        let extra = if status == "DAILY_LIMIT_REACHED" {
                            r#","message":"You have already requested a realtime balance for this meter today.","lastRealtimeBalance":971.11,"lastRealtimeReceivedAt":"2026-11-16T00:09:13.056Z","nextEligibleAt":"2026-11-16T18:00:00Z""#.to_string()
                        } else {
                            String::new()
                        };
                        Response::from_string(format!(
                            r#"{{"enabled":true,"meters":[{{"accountId":41810,"accountNo":"41167724","status":"{status}"{extra}}}]}}"#
                        ))
                    } else if p == "/api/accounts/41810/realtime-balance/req-1" {
                        // First poll: still waiting. Second poll: the meter has answered.
                        if state.realtime_polls.fetch_add(1, Ordering::SeqCst) == 0 {
                            Response::from_string(r#"{"status":"IN_PROGRESS","requestId":"req-1"}"#)
                        } else {
                            Response::from_string(
                                r#"{"status":"SUCCESS","balance":971.11,"receivedAt":"2026-11-16T00:09:13.056Z"}"#,
                            )
                        }
                    } else {
                        Response::from_string(r#"{"errorCode":"FORBIDDEN"}"#).with_status_code(403)
                    }
                }
                (Method::Post, "/api/accounts/41810/realtime-balance") => {
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).expect("body");
                    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
                    let valid = state.valid_session.lock().expect("lock").clone();
                    let key = json["idempotencyKey"].as_str().unwrap_or("");
                    if session.as_deref() != Some(valid.as_str()) {
                        Response::from_string("{}").with_status_code(401)
                    } else if xsrf_header.is_none() || xsrf_header != xsrf_cookie || key.len() != 36
                    {
                        Response::from_string("{}").with_status_code(403)
                    } else {
                        state.realtime_posts.fetch_add(1, Ordering::SeqCst);
                        Response::from_string(r#"{"status":"IN_PROGRESS","requestId":"req-1"}"#)
                    }
                }
                _ => Response::from_string("{}").with_status_code(404),
            };
            let _ = request.respond(response);
        }
    });
    format!("http://{addr}/api")
}

fn client(base: &str, password: &str) -> DescoClient {
    DescoClient::new(base, Duration::from_secs(5), 0, false)
        .expect("client")
        .with_credentials(Credentials::new("01700000000", password))
        .expect("portal")
}

fn account(n: &str) -> Identifier {
    Identifier::parse(n).expect("valid identifier")
}

#[test]
fn signs_in_once_and_reads_balance_by_account_or_meter_number() {
    let state = Arc::new(Portal::default());
    let base = start_portal("secret", Arc::clone(&state));
    let api = client(&base, "secret");

    let by_account = api.balance(&account("41167724"), None).expect("balance");
    assert!((by_account.balance - 362.91).abs() < 1e-9);
    assert_eq!(by_account.month_consumption, Some(370.83));
    let by_meter = api
        .balance(&account("030119013826"), None)
        .expect("balance");
    assert!((by_meter.balance - 362.91).abs() < 1e-9);
    assert_eq!(state.logins.load(Ordering::SeqCst), 1, "session is reused");
}

#[test]
fn signs_in_again_when_the_session_expires() {
    let state = Arc::new(Portal::default());
    let base = start_portal("secret", Arc::clone(&state));
    let api = client(&base, "secret");
    api.balance(&account("41167724"), None).expect("first read");

    // The portal forgets the session, as it does after 12 hours.
    *state.valid_session.lock().expect("lock") = "expired".to_string();
    api.balance(&account("41167724"), None)
        .expect("read after expiry");
    assert_eq!(state.logins.load(Ordering::SeqCst), 2);
}

#[test]
fn wrong_password_is_reported_as_login_failure_without_the_password() {
    let state = Arc::new(Portal::default());
    let base = start_portal("secret", state);
    let err = client(&base, "wrong-password")
        .balance(&account("41167724"), None)
        .expect_err("login must fail");
    assert!(matches!(err, ApiError::LoginFailed(_)), "{err:?}");
    assert!(!format!("{err:?} {err}").contains("wrong-password"));
    assert!(err.user_message().contains("sign in"));
}

#[test]
fn account_not_linked_to_the_login_is_not_found() {
    let state = Arc::new(Portal::default());
    let base = start_portal("secret", state);
    let err = client(&base, "secret")
        .balance(&account("99999999"), None)
        .expect_err("unlinked");
    assert!(matches!(err, ApiError::NotFound { .. }), "{err:?}");
    // A link that is no longer active is not offered either.
    let err = client(&base, "secret")
        .balance(&account("11111111"), None)
        .expect_err("removed link");
    assert!(matches!(err, ApiError::NotFound { .. }), "{err:?}");
}

#[test]
fn credentials_debug_output_hides_the_password() {
    let text = format!("{:?}", Credentials::new("01700000000", "hunter2-test-only"));
    assert!(text.contains("01700000000"));
    assert!(!text.contains("hunter2-test-only"));
}

const FAST: Duration = Duration::from_millis(10);

#[test]
fn realtime_balance_is_requested_then_polled_until_the_meter_answers() {
    let state = Portal::with_realtime_status("AVAILABLE");
    let base = start_portal("secret", Arc::clone(&state));
    let api = client(&base, "secret");

    let result = api
        .realtime_balance(&account("41167724"), Duration::from_secs(5), FAST)
        .expect("realtime");
    assert_eq!(
        result,
        Realtime::Fresh {
            balance: 971.11,
            received_at: "2026-11-16T00:09:13.056Z".to_string()
        }
    );
    assert_eq!(
        state.realtime_posts.load(Ordering::SeqCst),
        1,
        "exactly one attempt used"
    );
    assert_eq!(state.realtime_polls.load(Ordering::SeqCst), 2);
}

#[test]
fn realtime_is_not_requested_when_the_daily_limit_is_used_up() {
    let state = Portal::with_realtime_status("DAILY_LIMIT_REACHED");
    let base = start_portal("secret", Arc::clone(&state));
    let api = client(&base, "secret");

    let result = api
        .realtime_balance(&account("41167724"), Duration::from_secs(5), FAST)
        .expect("realtime");
    let Realtime::Unavailable {
        reason,
        last,
        retry_at,
    } = result
    else {
        panic!("expected unavailable, got {result:?}");
    };
    assert!(reason.contains("limit"), "{reason}");
    assert_eq!(retry_at.as_deref(), Some("2026-11-16T18:00:00Z"));
    assert_eq!(
        last.map(|(b, _)| b),
        Some(971.11),
        "last known reading is kept"
    );
    assert_eq!(
        state.realtime_posts.load(Ordering::SeqCst),
        0,
        "no attempt may be spent"
    );
}

#[test]
fn realtime_gives_up_politely_when_the_meter_never_answers() {
    let state = Portal::with_realtime_status("AVAILABLE");
    let base = start_portal("secret", Arc::clone(&state));
    // The mock answers on the 2nd poll; a zero wait stops before that.
    let result = client(&base, "secret")
        .realtime_balance(&account("41167724"), Duration::ZERO, FAST)
        .expect("realtime");
    assert!(
        matches!(&result, Realtime::Unavailable { reason, .. } if reason.contains("did not answer")),
        "{result:?}"
    );
}

#[test]
fn balance_prefers_a_realtime_reading_taken_after_the_daily_snapshot() {
    // Snapshot: 362.91 for 2026-11-15. The portal's last realtime reading (971.11, taken on
    // 2026-11-16 Dhaka time) is newer, as after a recharge.
    let state = Portal::with_realtime_status("DAILY_LIMIT_REACHED");
    let base = start_portal("secret", state);
    let b = client(&base, "secret")
        .balance(&account("41167724"), None)
        .expect("balance");
    assert!((b.balance - 971.11).abs() < 1e-9, "{b:?}");
    assert_eq!(b.reading_time, "2026-11-16 06:09");
}

#[test]
fn balance_keeps_the_snapshot_when_there_is_no_newer_realtime_reading() {
    let state = Portal::with_realtime_status("AVAILABLE");
    let base = start_portal("secret", state);
    let b = client(&base, "secret")
        .balance(&account("41167724"), None)
        .expect("balance");
    assert!((b.balance - 362.91).abs() < 1e-9, "{b:?}");
    assert_eq!(b.reading_time, "2026-11-15");
}
