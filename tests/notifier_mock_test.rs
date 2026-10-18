//! Delivery tests against a local fake provider. They check request shape (URL, auth, body),
//! not real delivery: no WhatsApp or Telegram account is involved.

use std::sync::{Arc, Mutex};
use std::thread;

use desco_sentinel::config::{Env, Settings};
use desco_sentinel::desco_api::Balance;
use desco_sentinel::domain::Backend;
use desco_sentinel::formatter::alert_message;
use desco_sentinel::notifier::{Notifier, NotifyError};
use serde_json::Value;
use tiny_http::{Response, Server};

#[derive(Debug, Clone)]
struct Seen {
    url: String,
    auth: Option<String>,
    api_key: Option<String>,
    body: Value,
}

/// `respond(n, body)` decides the status for the n-th request (0-based).
fn start_mock(
    respond: impl Fn(usize, &Value) -> u16 + Send + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let server = Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_ip().expect("ip");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for (n, mut request) in server.incoming_requests().enumerate() {
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let header = |name: &str| {
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                    .map(|h| h.value.as_str().to_string())
            };
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            log.lock().expect("lock").push(Seen {
                url: request.url().to_string(),
                auth: header("Authorization"),
                api_key: header("X-Api-Key"),
                body: body.clone(),
            });
            let status = respond(n, &body);
            let _ = request.respond(Response::from_string("{}").with_status_code(status));
        }
    });
    (format!("http://{addr}"), seen)
}

fn notifier(pairs: &[(&str, &str)]) -> Notifier {
    let env = Env::from_pairs(pairs.iter().copied());
    let settings = Settings::from_env(&env).expect("settings");
    Notifier::from_env(&env, &settings).expect("notifier")
}

fn sample_alert() -> desco_sentinel::formatter::Alert {
    let balance = Balance {
        account_no: "98765421".to_string(),
        meter_no: "667788990011".to_string(),
        balance: 95.5,
        month_consumption: Some(10.0),
        reading_time: "2026-10-18".to_string(),
        backend: Backend::Unified,
    };
    alert_message("Apt 1B", 150.0, &balance, false, None)
}

#[test]
fn meta_sends_bearer_authenticated_text_to_digits_only_number() {
    let (base, seen) = start_mock(|_, _| 200);
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_meta"),
        ("WA_PHONE_NUMBER_ID", "555"),
        ("WA_ACCESS_TOKEN", "tok"),
        ("RECIPIENT_PHONE", "+880 1700-000000"),
        ("WA_GRAPH_BASE", &base),
        ("WA_GRAPH_VERSION", "v99.0"),
    ]);
    n.send_alert(&sample_alert(), None).expect("sent");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url, "/v99.0/555/messages");
    assert_eq!(seen[0].auth.as_deref(), Some("Bearer tok"));
    assert_eq!(seen[0].body["to"], "8801700000000");
    assert_eq!(seen[0].body["type"], "text");
    assert!(
        seen[0].body["text"]["body"]
            .as_str()
            .expect("text")
            .contains("LOW BALANCE")
    );
}

#[test]
fn meta_uses_the_template_when_configured() {
    let (base, seen) = start_mock(|_, _| 200);
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_meta"),
        ("WA_PHONE_NUMBER_ID", "555"),
        ("WA_ACCESS_TOKEN", "tok"),
        ("RECIPIENT_PHONE", "8801700000000"),
        ("WA_GRAPH_BASE", &base),
        ("WA_TEMPLATE_NAME", "low_balance_alert"),
    ]);
    n.send_alert(&sample_alert(), None).expect("sent");
    let seen = seen.lock().expect("lock");
    let body = &seen[0].body;
    assert_eq!(body["type"], "template");
    assert_eq!(body["template"]["name"], "low_balance_alert");
    assert_eq!(body["template"]["language"]["code"], "en");
    let params = body["template"]["components"][0]["parameters"]
        .as_array()
        .expect("params");
    let texts: Vec<&str> = params.iter().filter_map(|p| p["text"].as_str()).collect();
    assert_eq!(texts[0], "Apt 1B");
    assert_eq!(texts[1], "95.50");
    assert_eq!(texts[2], "150.00");
    assert!(!texts[3].contains('\n'));
}

#[test]
fn tenant_gets_a_copy_and_tenant_failure_is_not_fatal() {
    let (base, seen) = start_mock(|n, _| if n == 0 { 200 } else { 400 });
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_meta"),
        ("WA_PHONE_NUMBER_ID", "555"),
        ("WA_ACCESS_TOKEN", "tok"),
        ("RECIPIENT_PHONE", "8801700000000"),
        ("WA_GRAPH_BASE", &base),
    ]);
    n.send_alert(&sample_alert(), Some("8801811111111"))
        .expect("group delivery succeeded, tenant failure only logged");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].body["to"], "8801811111111");
}

#[test]
fn rejected_default_delivery_is_an_error() {
    let (base, _) = start_mock(|_, _| 401);
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_meta"),
        ("WA_PHONE_NUMBER_ID", "555"),
        ("WA_ACCESS_TOKEN", "bad"),
        ("RECIPIENT_PHONE", "8801700000000"),
        ("WA_GRAPH_BASE", &base),
    ]);
    let err = n.send_alert(&sample_alert(), None).expect_err("rejected");
    assert!(
        matches!(err, NotifyError::Rejected { status: 401, .. }),
        "{err:?}"
    );
}

#[test]
fn missing_recipient_is_reported() {
    let (base, _) = start_mock(|_, _| 200);
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_meta"),
        ("WA_PHONE_NUMBER_ID", "555"),
        ("WA_ACCESS_TOKEN", "tok"),
        ("WA_GRAPH_BASE", &base),
    ]);
    assert!(matches!(
        n.send_alert(&sample_alert(), None),
        Err(NotifyError::NotConfigured(_))
    ));
}

#[test]
fn waha_posts_chat_id_text_and_api_key() {
    let (base, seen) = start_mock(|_, _| 201);
    let url = format!("{base}/api/sendText");
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_webhook"),
        ("WA_WEBHOOK_URL", &url),
        ("WA_WEBHOOK_TOKEN", "key"),
        ("WA_CHAT_ID", "123-456@g.us"),
    ]);
    n.send_alert(&sample_alert(), None).expect("sent");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen[0].url, "/api/sendText");
    assert_eq!(seen[0].api_key.as_deref(), Some("key"));
    assert_eq!(seen[0].body["chatId"], "123-456@g.us");
    assert_eq!(seen[0].body["session"], "default");
    assert!(
        seen[0].body["text"]
            .as_str()
            .expect("text")
            .contains("APT 1B")
    );
}

#[test]
fn green_api_uses_message_field_and_normalises_phone_to_chat_id() {
    let (base, seen) = start_mock(|_, _| 200);
    let url = format!("{base}/waInstance1/sendMessage/token");
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_webhook"),
        ("WA_WEBHOOK_FORMAT", "green"),
        ("WA_WEBHOOK_URL", &url),
        ("WA_CHAT_ID", "8801700000000"),
    ]);
    n.send_text("8801700000000", "hello").expect("sent");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen[0].body["chatId"], "8801700000000@c.us");
    assert_eq!(seen[0].body["message"], "hello");
    assert!(seen[0].api_key.is_none());
}

#[test]
fn telegram_retries_as_plain_text_when_markdown_is_rejected() {
    let (base, seen) = start_mock(|n, _| if n == 0 { 400 } else { 200 });
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "telegram"),
        ("TELEGRAM_BOT_TOKEN", "123:abc"),
        ("TELEGRAM_CHAT_ID", "-100200"),
        ("TELEGRAM_API_BASE", &base),
    ]);
    n.send_alert(&sample_alert(), None)
        .expect("sent on the second try");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].url, "/bot123:abc/sendMessage");
    assert_eq!(seen[0].body["parse_mode"], "Markdown");
    assert!(seen[1].body.get("parse_mode").is_none());
    assert_eq!(seen[1].body["chat_id"], "-100200");
}

#[test]
fn errors_do_not_leak_the_bot_token() {
    // Port 9 is closed: the connection fails and the error text must not contain the token.
    let n = notifier(&[
        ("NOTIFY_PROVIDER", "telegram"),
        ("TELEGRAM_BOT_TOKEN", "999:SECRET"),
        ("TELEGRAM_CHAT_ID", "1"),
        ("TELEGRAM_API_BASE", "http://127.0.0.1:9"),
    ]);
    let err = n.send_text("1", "x").expect_err("closed port");
    assert!(!err.to_string().contains("SECRET"), "{err}");
}

fn two_recipient_notifier(base: &str) -> Notifier {
    notifier(&[
        ("NOTIFY_PROVIDER", "whatsapp_meta"),
        ("WA_PHONE_NUMBER_ID", "555"),
        ("WA_ACCESS_TOKEN", "tok"),
        ("RECIPIENT_PHONE", "8801700000001,8801700000002"),
        ("WA_GRAPH_BASE", base),
    ])
}

#[test]
fn both_recipients_receive_the_alert_in_order() {
    let (base, seen) = start_mock(|_, _| 200);
    two_recipient_notifier(&base)
        .send_alert(&sample_alert(), None)
        .expect("sent");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body["to"], "8801700000001", "primary first");
    assert_eq!(seen[1].body["to"], "8801700000002");
}

#[test]
fn alert_counts_as_delivered_if_only_one_recipient_gets_it() {
    // Primary rejected, secondary accepted: success, so the next run will not re-send.
    let (base, seen) = start_mock(|n, _| if n == 0 { 400 } else { 200 });
    two_recipient_notifier(&base)
        .send_alert(&sample_alert(), None)
        .expect("secondary delivery is enough");
    assert_eq!(
        seen.lock().expect("lock").len(),
        2,
        "secondary still attempted"
    );

    // Primary accepted, secondary rejected: also success.
    let (base, _) = start_mock(|n, _| if n == 0 { 200 } else { 400 });
    two_recipient_notifier(&base)
        .send_alert(&sample_alert(), None)
        .expect("primary delivery is enough");
}

#[test]
fn alert_fails_only_when_nobody_got_it() {
    let (base, _) = start_mock(|_, _| 500);
    let err = two_recipient_notifier(&base)
        .send_alert(&sample_alert(), None)
        .expect_err("all rejected");
    assert!(
        matches!(err, NotifyError::Rejected { status: 500, .. }),
        "{err:?}"
    );
}
