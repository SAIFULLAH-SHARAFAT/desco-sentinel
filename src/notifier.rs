//! Notification dispatcher: Meta WhatsApp Cloud API, WAHA / Green-API, Telegram or console.
//! The channel is chosen with `NOTIFY_PROVIDER`; nothing else in the program cares which one.

use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::warn;

use crate::config::{Env, Provider, Settings, digits_only};
use crate::formatter::Alert;

/// WhatsApp rejects text bodies above 4096 characters.
const MAX_MESSAGE_CHARS: usize = 4000;

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("{0}")]
    NotConfigured(String),
    #[error("request failed: {0}")]
    Http(String),
    #[error("provider answered HTTP {status}: {body}")]
    Rejected { status: u16, body: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebhookFormat {
    Waha,
    Green,
}

enum Channel {
    Meta {
        url: String,
        token: String,
        recipients: Vec<String>,
        template: Option<(String, String)>,
    },
    Webhook {
        url: String,
        token: Option<String>,
        session: String,
        format: WebhookFormat,
        recipients: Vec<String>,
    },
    Telegram {
        url: String,
        recipients: Vec<String>,
    },
    Console,
}

pub struct Notifier {
    channel: Channel,
    http: Client,
    notify_tenants: bool,
}

fn required(env: &Env, key: &str, provider: &str) -> Result<String, NotifyError> {
    env.get(key).map(ToString::to_string).ok_or_else(|| {
        NotifyError::NotConfigured(format!("{key} is required for NOTIFY_PROVIDER={provider}"))
    })
}

fn list(env: &Env, key: &str) -> Vec<String> {
    env.get(key)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl Notifier {
    pub fn from_env(env: &Env, settings: &Settings) -> Result<Self, NotifyError> {
        let channel = match settings.provider {
            Provider::Console => Channel::Console,
            Provider::WhatsAppMeta => {
                let phone_id = required(env, "WA_PHONE_NUMBER_ID", "whatsapp_meta")?;
                let token = required(env, "WA_ACCESS_TOKEN", "whatsapp_meta")?;
                let version = env.get_or("WA_GRAPH_VERSION", "v23.0");
                let template = env.get("WA_TEMPLATE_NAME").map(|name| {
                    (
                        name.to_string(),
                        env.get_or("WA_TEMPLATE_LANG", "en").to_string(),
                    )
                });
                Channel::Meta {
                    url: format!(
                        "{}/{version}/{phone_id}/messages",
                        env.get_or("WA_GRAPH_BASE", "https://graph.facebook.com")
                            .trim_end_matches('/')
                    ),
                    token,
                    recipients: list(env, "RECIPIENT_PHONE"),
                    template,
                }
            }
            Provider::WhatsAppWebhook => {
                let format = match env
                    .get_or("WA_WEBHOOK_FORMAT", "waha")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "green" | "green-api" => WebhookFormat::Green,
                    _ => WebhookFormat::Waha,
                };
                Channel::Webhook {
                    url: required(env, "WA_WEBHOOK_URL", "whatsapp_webhook")?,
                    token: env.get("WA_WEBHOOK_TOKEN").map(ToString::to_string),
                    session: env.get_or("WA_WEBHOOK_SESSION", "default").to_string(),
                    format,
                    recipients: list(env, "WA_CHAT_ID"),
                }
            }
            Provider::Telegram => {
                let token = required(env, "TELEGRAM_BOT_TOKEN", "telegram")?;
                Channel::Telegram {
                    url: format!(
                        "{}/bot{token}/sendMessage",
                        env.get_or("TELEGRAM_API_BASE", "https://api.telegram.org")
                            .trim_end_matches('/')
                    ),
                    recipients: list(env, "TELEGRAM_CHAT_ID"),
                }
            }
        };
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        Ok(Self {
            channel,
            http,
            notify_tenants: settings.notify_tenants,
        })
    }

    /// Telegram notifier built from `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID`, whatever
    /// `NOTIFY_PROVIDER` says. Used by the Telegram bot, which always replies on Telegram.
    pub fn telegram(env: &Env, settings: &Settings) -> Result<Self, NotifyError> {
        let mut forced = settings.clone();
        forced.provider = Provider::Telegram;
        Self::from_env(env, &forced)
    }

    /// Console-only notifier (used by `--dry-run` style flows and tests).
    #[must_use]
    pub fn console() -> Self {
        Self {
            channel: Channel::Console,
            http: Client::new(),
            notify_tenants: false,
        }
    }

    #[must_use]
    pub fn is_console(&self) -> bool {
        matches!(self.channel, Channel::Console)
    }

    #[must_use]
    pub fn notify_tenants(&self) -> bool {
        self.notify_tenants
    }

    fn default_recipients(&self) -> &[String] {
        match &self.channel {
            Channel::Meta { recipients, .. }
            | Channel::Webhook { recipients, .. }
            | Channel::Telegram { recipients, .. } => recipients,
            Channel::Console => &[],
        }
    }

    fn post_json(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: &Value,
    ) -> Result<(), NotifyError> {
        let mut request = self.http.post(url).json(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        // `without_url` keeps bot tokens that live in the URL out of logs.
        let response = request
            .send()
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body: String = response
            .text()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        Err(NotifyError::Rejected {
            status: status.as_u16(),
            body,
        })
    }

    /// Sends free text to one recipient (a phone number, chat id or Telegram chat).
    pub fn send_text(&self, to: &str, text: &str) -> Result<(), NotifyError> {
        for part in split_message(text, MAX_MESSAGE_CHARS) {
            self.send_text_part(to, &part)?;
        }
        Ok(())
    }

    fn send_text_part(&self, to: &str, text: &str) -> Result<(), NotifyError> {
        match &self.channel {
            Channel::Console => {
                println!("{text}\n");
                Ok(())
            }
            Channel::Meta { url, token, .. } => {
                let body = json!({
                    "messaging_product": "whatsapp",
                    "to": digits_only(to),
                    "type": "text",
                    "text": { "body": text },
                });
                self.post_json(url, &[("Authorization", format!("Bearer {token}"))], &body)
            }
            Channel::Webhook {
                url,
                token,
                session,
                format,
                ..
            } => {
                let chat_id = chat_id(to);
                let (body, headers) = match format {
                    WebhookFormat::Waha => (
                        json!({ "chatId": chat_id, "text": text, "session": session }),
                        token
                            .iter()
                            .map(|t| ("X-Api-Key", t.clone()))
                            .collect::<Vec<_>>(),
                    ),
                    WebhookFormat::Green => {
                        (json!({ "chatId": chat_id, "message": text }), Vec::new())
                    }
                };
                self.post_json(url, &headers, &body)
            }
            Channel::Telegram { url, .. } => {
                // Telegram's legacy Markdown understands *bold*, _italic_ and ``` blocks.
                let markdown = json!({ "chat_id": to, "text": text, "parse_mode": "Markdown" });
                match self.post_json(url, &[], &markdown) {
                    Err(NotifyError::Rejected { status: 400, .. }) => {
                        // Unbalanced markup (for example '_' in a unit name): send as plain text.
                        self.post_json(url, &[], &json!({ "chat_id": to, "text": text }))
                    }
                    other => other,
                }
            }
        }
    }

    /// Delivers a low-balance alert to the default recipients and, if given, to a tenant.
    /// Failure to reach the default recipients is an error; a failing tenant is only logged.
    pub fn send_alert(&self, alert: &Alert, tenant: Option<&str>) -> Result<(), NotifyError> {
        if self.is_console() {
            println!("{}\n", alert.text);
            return Ok(());
        }
        let defaults = self.default_recipients();
        if defaults.is_empty() && tenant.is_none() {
            return Err(NotifyError::NotConfigured(
                "no recipient configured (RECIPIENT_PHONE / WA_CHAT_ID / TELEGRAM_CHAT_ID)"
                    .to_string(),
            ));
        }
        // Recipients are tried in order (primary first). The alert counts as delivered when at
        // least one of them got it; otherwise a retry would re-send to those who already did.
        let mut delivered = 0;
        let mut last_error = None;
        for to in defaults {
            match self.send_alert_to(to, alert) {
                Ok(()) => delivered += 1,
                Err(e) => {
                    warn!(error = %e, "alert delivery to a recipient failed");
                    last_error = Some(e);
                }
            }
        }
        if delivered == 0
            && let Some(e) = last_error
        {
            return Err(e);
        }
        if let Some(phone) = tenant
            && let Err(e) = self.send_alert_to(phone, alert)
        {
            warn!(error = %e, "tenant notification failed");
        }
        Ok(())
    }

    fn send_alert_to(&self, to: &str, alert: &Alert) -> Result<(), NotifyError> {
        // Outside the 24h customer-service window Meta only accepts approved templates.
        if let Channel::Meta {
            url,
            token,
            template: Some((name, lang)),
            ..
        } = &self.channel
        {
            let params: Vec<Value> = [
                &alert.unit,
                &alert.balance,
                &alert.threshold,
                &alert.summary,
            ]
            .iter()
            .map(|text| json!({ "type": "text", "text": text }))
            .collect();
            let body = json!({
                "messaging_product": "whatsapp",
                "to": digits_only(to),
                "type": "template",
                "template": {
                    "name": name,
                    "language": { "code": lang },
                    "components": [{ "type": "body", "parameters": params }],
                },
            });
            return self.post_json(url, &[("Authorization", format!("Bearer {token}"))], &body);
        }
        self.send_text(to, &alert.text)
    }
}

/// `8801700000000` -> `8801700000000@c.us`; ids that already contain `@` are kept.
fn chat_id(to: &str) -> String {
    if to.contains('@') {
        to.to_string()
    } else {
        format!("{}@c.us", digits_only(to))
    }
}

/// Splits on line boundaries so no part exceeds `max_chars`.
#[must_use]
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let line_len = line.chars().count();
        if !current.is_empty() && current.chars().count() + line_len + 1 > max_chars {
            parts.push(std::mem::take(&mut current));
        }
        if line_len > max_chars {
            // A single oversized line: hard-split it.
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(max_chars) {
                parts.push(chunk.iter().collect());
            }
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_part() {
        assert_eq!(split_message("a\nb", 100), vec!["a\nb".to_string()]);
    }

    #[test]
    fn long_text_splits_on_lines() {
        let text = (0..10)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let parts = split_message(&text, 20);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.chars().count() <= 20));
        assert_eq!(parts.join("\n"), text);
    }

    #[test]
    fn oversized_line_is_hard_split() {
        let parts = split_message(&"x".repeat(25), 10);
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn chat_ids_are_normalised() {
        assert_eq!(chat_id("+880 1700-000000"), "8801700000000@c.us");
        assert_eq!(chat_id("123-456@g.us"), "123-456@g.us");
    }

    #[test]
    fn provider_requires_its_secrets() {
        let env = Env::from_pairs([("NOTIFY_PROVIDER", "telegram")]);
        let settings = Settings::from_env(&env).expect("settings");
        assert!(matches!(
            Notifier::from_env(&env, &settings),
            Err(NotifyError::NotConfigured(_))
        ));
    }
}
