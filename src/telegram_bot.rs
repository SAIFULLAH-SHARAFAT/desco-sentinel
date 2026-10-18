//! Two-way Telegram bot using long polling (`getUpdates`). It needs no public URL, so it
//! runs anywhere that can reach api.telegram.org.
//!
//! Only chats listed in `TELEGRAM_ALLOWED_CHATS` (default: `TELEGRAM_CHAT_ID`) get answers.
//! Anyone else is told their chat id, so the owner can add them to the list.

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::Env;
use crate::monitor::Monitor;
use crate::notifier::Notifier;
use crate::query_engine::handle_incoming_query;

/// Seconds Telegram holds a `getUpdates` request open when nothing arrives.
const POLL_SECONDS: u64 = 30;
/// Messages older than this were sent while the bot was down; answering them would confuse.
const MAX_MESSAGE_AGE_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TgMessage {
    pub update_id: i64,
    pub chat_id: String,
    pub text: String,
    /// Unix time the message was sent.
    pub date: i64,
}

/// Extracts a text message from one `getUpdates` entry. Edits, photos, joins etc. yield `None`.
#[must_use]
pub fn parse_update(update: &Value) -> Option<TgMessage> {
    let message = update.get("message")?;
    Some(TgMessage {
        update_id: update.get("update_id")?.as_i64()?,
        chat_id: message.pointer("/chat/id")?.as_i64()?.to_string(),
        text: message.get("text")?.as_str()?.to_string(),
        date: message.get("date").and_then(Value::as_i64).unwrap_or(0),
    })
}

fn split_ids(raw: Option<&str>) -> Vec<String> {
    raw.map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// Chats allowed to talk to the bot.
#[must_use]
pub fn allowed_chats(env: &Env) -> Vec<String> {
    let explicit = split_ids(env.get("TELEGRAM_ALLOWED_CHATS"));
    if explicit.is_empty() {
        split_ids(env.get("TELEGRAM_CHAT_ID"))
    } else {
        explicit
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

struct Api {
    http: Client,
    base: String,
}

impl Api {
    fn call(&self, method: &str, body: &Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(body)
            .send()
            // `without_url`: the bot token is part of the URL and must not reach logs.
            .map_err(|e| anyhow!("telegram {method}: {}", e.without_url()))?;
        let status = response.status();
        let value: Value = response.json().map_err(|e| {
            anyhow!(
                "telegram {method}: bad response ({status}): {}",
                e.without_url()
            )
        })?;
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(value)
        } else {
            let why = value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            Err(anyhow!("telegram {method} refused ({status}): {why}"))
        }
    }

    /// Shows the command list in Telegram's menu. Best effort.
    fn publish_commands(&self) {
        let commands = json!({ "commands": [
            { "command": "status", "description": "Balance of every meter" },
            { "command": "low", "description": "Only meters with low balance" },
            { "command": "info", "description": "Directory of all meters" },
            { "command": "help", "description": "How to use this bot" },
        ]});
        if let Err(e) = self.call("setMyCommands", &commands) {
            warn!(error = %e, "could not publish the command menu");
        }
    }
}

/// Runs until the process stops. Errors from Telegram are retried with a growing pause.
pub fn run(monitor: &Monitor, notifier: &Notifier, env: &Env) -> Result<()> {
    let token = env
        .get("TELEGRAM_BOT_TOKEN")
        .context("TELEGRAM_BOT_TOKEN is required for the Telegram bot")?;
    let base = format!(
        "{}/bot{token}",
        env.get_or("TELEGRAM_API_BASE", "https://api.telegram.org")
            .trim_end_matches('/')
    );
    let api = Api {
        http: Client::builder()
            .timeout(Duration::from_secs(POLL_SECONDS + 15))
            .build()
            .map_err(|e| anyhow!("cannot build HTTP client: {}", e.without_url()))?,
        base,
    };
    let allowed = allowed_chats(env);
    if allowed.is_empty() {
        warn!("TELEGRAM_CHAT_ID / TELEGRAM_ALLOWED_CHATS is empty: nobody can use the bot");
    }
    api.publish_commands();
    info!(chats = allowed.len(), "telegram bot polling");

    let mut offset: i64 = 0;
    let mut pause = Duration::from_secs(2);
    loop {
        let request = json!({
            "offset": offset,
            "timeout": POLL_SECONDS,
            "allowed_updates": ["message"],
        });
        let updates = match api.call("getUpdates", &request) {
            Ok(v) => {
                pause = Duration::from_secs(2);
                v
            }
            Err(e) => {
                warn!(error = %e, ?pause, "telegram polling failed");
                thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        for update in updates
            .get("result")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                offset = offset.max(id + 1);
            }
            if let Some(message) = parse_update(update) {
                answer(monitor, notifier, &allowed, &message);
            }
        }
    }
}

fn answer(monitor: &Monitor, notifier: &Notifier, allowed: &[String], message: &TgMessage) {
    if now_unix() - message.date > MAX_MESSAGE_AGE_SECS {
        return;
    }
    let reply = if allowed.contains(&message.chat_id) {
        info!(text = %message.text.chars().take(40).collect::<String>(), "telegram query");
        handle_incoming_query(&message.text, monitor, true)
    } else {
        warn!(chat = %message.chat_id, "message from a chat that is not allowed");
        format!(
            "⛔ This bot is private.\nYour chat id is `{}`. Send it to the owner to get access.",
            message.chat_id
        )
    };
    if let Err(e) = notifier.send_text(&message.chat_id, &reply) {
        warn!(error = %e, "could not send telegram reply");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_messages_only() {
        let text = json!({ "update_id": 7, "message": { "date": 1000, "text": "status 825", "chat": { "id": 123_456_789_i64, "type": "private" } } });
        assert_eq!(
            parse_update(&text),
            Some(TgMessage {
                update_id: 7,
                chat_id: "123456789".to_string(),
                text: "status 825".to_string(),
                date: 1000
            })
        );
        let photo =
            json!({ "update_id": 8, "message": { "date": 1, "chat": { "id": 1 }, "photo": [] } });
        assert_eq!(parse_update(&photo), None);
        let edited =
            json!({ "update_id": 9, "edited_message": { "chat": { "id": 1 }, "text": "x" } });
        assert_eq!(parse_update(&edited), None);
    }

    #[test]
    fn group_chat_ids_are_negative_and_kept() {
        let update = json!({ "update_id": 1, "message": { "date": 1, "text": "/status", "chat": { "id": -1_001_234_567_890_i64 } } });
        assert_eq!(
            parse_update(&update).map(|m| m.chat_id),
            Some("-1001234567890".to_string())
        );
    }

    #[test]
    fn allow_list_defaults_to_alert_recipients() {
        let env = Env::from_pairs([("TELEGRAM_CHAT_ID", "111, 222")]);
        assert_eq!(
            allowed_chats(&env),
            vec!["111".to_string(), "222".to_string()]
        );
        let env = Env::from_pairs([
            ("TELEGRAM_CHAT_ID", "111"),
            ("TELEGRAM_ALLOWED_CHATS", "333"),
        ]);
        assert_eq!(allowed_chats(&env), vec!["333".to_string()]);
        assert_eq!(allowed_chats(&Env::default()), Vec::<String>::new());
    }
}
