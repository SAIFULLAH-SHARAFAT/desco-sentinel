//! Two-way WhatsApp bot: a small synchronous HTTP server.
//!
//! * `GET  /webhook`  Meta verification handshake
//! * `POST /webhook`  incoming messages from Meta Cloud API, WAHA or Green-API
//! * `GET  /health`   liveness probe
//!
//! The HTTP handler only parses and queues the message, then answers 200 at once (Meta retries
//! slow webhooks). A worker thread does the slow DESCO lookups and sends the reply. The queue is
//! bounded, so a flood of messages cannot exhaust memory.

use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

use anyhow::{Result, anyhow};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tiny_http::{Header, Method, Request, Response, Server};
use tracing::{info, warn};

use crate::config::Env;
use crate::monitor::Monitor;
use crate::notifier::Notifier;
use crate::query_engine::handle_incoming_query;

const QUEUE_SIZE: usize = 64;
const MAX_BODY_BYTES: u64 = 1_000_000;

/// One text message received from a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    /// Who wrote it (used for the allow-list).
    pub sender: String,
    /// Where the answer goes (the sender, or the group they wrote in).
    pub reply_to: String,
    pub text: String,
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

/// Extracts text messages from a Meta, WAHA or Green-API payload. Anything else yields nothing.
#[must_use]
pub fn parse_incoming(payload: &Value) -> Vec<Incoming> {
    let mut out = Vec::new();

    // Meta WhatsApp Cloud API: entry[].changes[].value.messages[]
    if str_at(payload, &["object"]) == Some("whatsapp_business_account") {
        let entries = payload
            .get("entry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for entry in entries {
            let changes = entry
                .get("changes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            for change in changes {
                let messages = change
                    .pointer("/value/messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten();
                for message in messages {
                    if str_at(message, &["type"]) != Some("text") {
                        continue;
                    }
                    if let (Some(from), Some(body)) = (
                        str_at(message, &["from"]),
                        str_at(message, &["text", "body"]),
                    ) {
                        out.push(Incoming {
                            sender: from.to_string(),
                            reply_to: from.to_string(),
                            text: body.to_string(),
                        });
                    }
                }
            }
        }
        return out;
    }

    // Green-API
    if str_at(payload, &["typeWebhook"]) == Some("incomingMessageReceived") {
        let text = str_at(payload, &["messageData", "textMessageData", "textMessage"])
            .or_else(|| str_at(payload, &["messageData", "extendedTextMessageData", "text"]));
        if let (Some(chat), Some(text)) = (str_at(payload, &["senderData", "chatId"]), text) {
            let sender = str_at(payload, &["senderData", "sender"]).unwrap_or(chat);
            out.push(Incoming {
                sender: sender.to_string(),
                reply_to: chat.to_string(),
                text: text.to_string(),
            });
        }
        return out;
    }

    // WAHA
    if str_at(payload, &["event"]) == Some("message") {
        let message = payload.get("payload").unwrap_or(&Value::Null);
        let from_me = message
            .get("fromMe")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let (false, Some(chat), Some(body)) = (
            from_me,
            str_at(message, &["from"]),
            str_at(message, &["body"]),
        ) {
            let sender = str_at(message, &["participant"]).unwrap_or(chat);
            out.push(Incoming {
                sender: sender.to_string(),
                reply_to: chat.to_string(),
                text: body.to_string(),
            });
        }
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Checks Meta's `X-Hub-Signature-256: sha256=<hex>` header (constant-time comparison).
#[must_use]
pub fn verify_meta_signature(app_secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex) = header.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Some(expected) = hex_decode(hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Answers Meta's `hub.mode=subscribe` handshake: returns the challenge when the token matches.
#[must_use]
pub fn verify_challenge(query: &[(String, String)], verify_token: &str) -> Option<String> {
    let get = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    if get("hub.mode") == Some("subscribe") && get("hub.verify_token") == Some(verify_token) {
        get("hub.challenge").map(ToString::to_string)
    } else {
        None
    }
}

struct Secrets {
    meta_verify_token: Option<String>,
    meta_app_secret: Option<String>,
    inbound_token: Option<String>,
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(body.to_string()).with_status_code(status);
    if let Ok(header) = Header::from_bytes("Content-Type", "application/json") {
        response = response.with_header(header);
    }
    response
}

fn respond(request: Request, response: Response<std::io::Cursor<Vec<u8>>>) {
    if let Err(e) = request.respond(response) {
        warn!(error = %e, "could not write HTTP response");
    }
}

fn handle_get(request: Request, url: &reqwest::Url, secrets: &Secrets) {
    match url.path() {
        "/health" => respond(request, json_response(200, &json!({ "status": "ok" }))),
        "/webhook" => {
            let query: Vec<(String, String)> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let challenge = secrets
                .meta_verify_token
                .as_deref()
                .and_then(|token| verify_challenge(&query, token));
            match challenge {
                Some(c) => respond(request, Response::from_string(c)),
                None => respond(
                    request,
                    json_response(403, &json!({ "error": "verification failed" })),
                ),
            }
        }
        _ => respond(
            request,
            json_response(404, &json!({ "error": "not found" })),
        ),
    }
}

fn handle_post(
    mut request: Request,
    url: &reqwest::Url,
    secrets: &Secrets,
    queue: &SyncSender<Incoming>,
) {
    if url.path() != "/webhook" {
        respond(
            request,
            json_response(404, &json!({ "error": "not found" })),
        );
        return;
    }
    let mut body = Vec::new();
    if request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut body)
        .is_err()
    {
        respond(
            request,
            json_response(400, &json!({ "error": "unreadable body" })),
        );
        return;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        respond(
            request,
            json_response(400, &json!({ "error": "invalid JSON" })),
        );
        return;
    };

    let is_meta = str_at(&payload, &["object"]) == Some("whatsapp_business_account");
    let authentic = if is_meta {
        match &secrets.meta_app_secret {
            Some(secret) => header_value(&request, "X-Hub-Signature-256")
                .is_some_and(|sig| verify_meta_signature(secret, &body, &sig)),
            None => true,
        }
    } else {
        match &secrets.inbound_token {
            Some(expected) => {
                let from_query = url
                    .query_pairs()
                    .find(|(k, _)| k == "token")
                    .map(|(_, v)| v.into_owned());
                let supplied = from_query
                    .or_else(|| header_value(&request, "X-Webhook-Token"))
                    .or_else(|| header_value(&request, "X-Api-Key"));
                supplied.as_deref() == Some(expected.as_str())
            }
            None => true,
        }
    };
    if !authentic {
        warn!("rejected webhook call with bad signature or token");
        respond(
            request,
            json_response(401, &json!({ "error": "unauthorized" })),
        );
        return;
    }

    for incoming in parse_incoming(&payload) {
        match queue.try_send(incoming) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => warn!("message queue full, dropping message"),
            Err(TrySendError::Disconnected(_)) => warn!("worker stopped, dropping message"),
        }
    }
    respond(
        request,
        json_response(200, &json!({ "status": "received" })),
    );
}

fn worker(monitor: &Monitor, notifier: &Notifier, queue: &Receiver<Incoming>) {
    for incoming in queue {
        let settings = monitor.settings();
        if !settings.sender_allowed(&incoming.sender) {
            warn!("ignored message from a sender that is not on the allow-list");
            continue;
        }
        info!(text = %incoming.text.chars().take(40).collect::<String>(), "query received");
        let reply = handle_incoming_query(&incoming.text, monitor, settings.pii_allowed());
        if let Err(e) = notifier.send_text(&incoming.reply_to, &reply) {
            warn!(error = %e, "could not send reply");
        }
    }
}

fn scheduler(monitor: &Monitor, notifier: &Notifier) {
    let interval = monitor.settings().check_interval;
    loop {
        let report = monitor.run_check(notifier, false);
        info!(
            checked = report.checked,
            low = report.low,
            sent = report.alerts_sent,
            unreadable = report.unreadable,
            "scheduled check finished"
        );
        thread::sleep(interval);
    }
}

/// Runs until the process is stopped.
pub fn serve(monitor: &Arc<Monitor>, notifier: &Arc<Notifier>, env: &Env) -> Result<()> {
    let settings = monitor.settings().clone();
    let secrets = Secrets {
        meta_verify_token: env.get("WA_VERIFY_TOKEN").map(ToString::to_string),
        meta_app_secret: env.get("WA_APP_SECRET").map(ToString::to_string),
        inbound_token: env.get("WEBHOOK_SECRET").map(ToString::to_string),
    };
    if settings.allowed_senders.is_empty() {
        warn!(
            "ALLOWED_SENDERS is not set: anyone can query the bot, customer names and phones are hidden"
        );
    }
    if secrets.meta_app_secret.is_none() && secrets.inbound_token.is_none() {
        warn!(
            "neither WA_APP_SECRET nor WEBHOOK_SECRET is set: incoming webhooks are not authenticated"
        );
    }

    let server = Server::http(("0.0.0.0", settings.port))
        .map_err(|e| anyhow!("cannot listen on port {}: {e}", settings.port))?;
    info!(port = settings.port, "webhook server listening");

    let (tx, rx) = mpsc::sync_channel::<Incoming>(QUEUE_SIZE);
    {
        let (monitor, notifier) = (Arc::clone(monitor), Arc::clone(notifier));
        thread::Builder::new()
            .name("query-worker".to_string())
            .spawn(move || worker(&monitor, &notifier, &rx))?;
    }
    if settings.check_interval.is_zero() {
        info!("periodic checks disabled (CHECK_INTERVAL_HOURS=0)");
    } else {
        let (monitor, notifier) = (Arc::clone(monitor), Arc::clone(notifier));
        thread::Builder::new()
            .name("scheduler".to_string())
            .spawn(move || scheduler(&monitor, &notifier))?;
    }

    for request in server.incoming_requests() {
        let method = request.method().clone();
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost{}", request.url())) else {
            respond(request, json_response(400, &json!({ "error": "bad url" })));
            continue;
        };
        match method {
            Method::Get => handle_get(request, &url, &secrets),
            Method::Post => handle_post(request, &url, &secrets, &tx),
            _ => respond(
                request,
                json_response(405, &json!({ "error": "method not allowed" })),
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meta_payload() {
        let payload = json!({
            "object": "whatsapp_business_account",
            "entry": [{ "changes": [{ "value": { "messages": [
                { "from": "8801700000000", "type": "text", "text": { "body": "421" } },
                { "from": "8801700000000", "type": "image" }
            ]}}]}]
        });
        assert_eq!(
            parse_incoming(&payload),
            vec![Incoming {
                sender: "8801700000000".to_string(),
                reply_to: "8801700000000".to_string(),
                text: "421".to_string()
            }]
        );
    }

    #[test]
    fn parses_waha_payload_and_ignores_own_messages() {
        let incoming = json!({
            "event": "message", "session": "default",
            "payload": { "from": "123-456@g.us", "participant": "8801700000000@c.us", "fromMe": false, "body": "status" }
        });
        let parsed = parse_incoming(&incoming);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].reply_to, "123-456@g.us");
        assert_eq!(parsed[0].sender, "8801700000000@c.us");

        let own = json!({ "event": "message", "payload": { "from": "x@c.us", "fromMe": true, "body": "status" } });
        assert_eq!(parse_incoming(&own), Vec::new());
    }

    #[test]
    fn parses_green_api_payload() {
        let payload = json!({
            "typeWebhook": "incomingMessageReceived",
            "senderData": { "chatId": "8801700000000@c.us", "sender": "8801700000000@c.us" },
            "messageData": { "typeMessage": "textMessage", "textMessageData": { "textMessage": "info" } }
        });
        let parsed = parse_incoming(&payload);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "info");
    }

    #[test]
    fn unknown_payload_yields_nothing() {
        assert_eq!(parse_incoming(&json!({ "hello": "world" })), Vec::new());
    }

    #[test]
    fn meta_challenge_needs_matching_token() {
        let query = vec![
            ("hub.mode".to_string(), "subscribe".to_string()),
            ("hub.verify_token".to_string(), "secret".to_string()),
            ("hub.challenge".to_string(), "12345".to_string()),
        ];
        assert_eq!(
            verify_challenge(&query, "secret"),
            Some("12345".to_string())
        );
        assert_eq!(verify_challenge(&query, "wrong"), None);
    }

    #[test]
    fn meta_signature_roundtrip() {
        let body = br#"{"object":"whatsapp_business_account"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"app-secret").expect("any key length works");
        mac.update(body);
        let hex = mac
            .finalize()
            .into_bytes()
            .iter()
            .fold(String::new(), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{b:02x}");
                acc
            });
        let header = format!("sha256={hex}");
        assert!(verify_meta_signature("app-secret", body, &header));
        assert!(!verify_meta_signature("other", body, &header));
        assert!(!verify_meta_signature("app-secret", b"tampered", &header));
        assert!(!verify_meta_signature("app-secret", body, "sha256=zz"));
    }
}
