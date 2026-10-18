//! Interactive WhatsApp query handling: `status`, `info`, last-3-digits, unit names, `help`.

use crate::clock;
use crate::config::Meter;
use crate::desco_api::Balance;
use crate::domain::Last3;
use crate::formatter::{
    CardData, InfoRow, StatusRow, help_text, whatsapp_card, whatsapp_info, whatsapp_status,
};
use crate::monitor::Monitor;

/// A parsed incoming message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Status,
    Info,
    Help,
    Last3(Last3),
    Name(String),
}

/// Most unit-name matches shown as full cards; more than that gets a short list.
const MAX_CARDS: usize = 4;

#[must_use]
pub fn classify(text: &str) -> Query {
    let cleaned = text
        .trim()
        .trim_start_matches(['/', '!', '.'])
        .trim()
        .to_lowercase();
    match cleaned.as_str() {
        "all" | "status" | "table" | "list" | "balance" | "balances" => Query::Status,
        "info" | "meters" | "directory" => Query::Info,
        "" | "help" | "menu" | "hi" | "hello" | "?" | "start" => Query::Help,
        other => match Last3::parse(other) {
            Ok(last3) => Query::Last3(last3),
            Err(_) => Query::Name(other.to_string()),
        },
    }
}

fn synced_now() -> String {
    clock::format_minute(clock::dhaka_now())
}

/// Produces the reply text for one incoming message. `show_pii` controls whether customer
/// names and phone numbers may appear.
pub fn handle_incoming_query(text: &str, monitor: &Monitor, show_pii: bool) -> String {
    match classify(text) {
        Query::Help => help_text(),
        Query::Status => status_reply(monitor),
        Query::Info => info_reply(monitor, show_pii),
        Query::Last3(last3) => {
            let by_digits = monitor.find_by_last3(&last3);
            if by_digits.is_empty() {
                // A unit could be literally named "101".
                let by_name = monitor.find_by_name(last3.as_str());
                if by_name.is_empty() {
                    return format!(
                        "No meter ends with *{}*.\n\n{}",
                        last3.as_str(),
                        help_text()
                    );
                }
                return cards_reply(monitor, &by_name, show_pii);
            }
            cards_reply(monitor, &by_digits, show_pii)
        }
        Query::Name(name) => {
            let matches = monitor.find_by_name(&name);
            if matches.is_empty() {
                return format!("No meter matches \"{name}\".\n\n{}", help_text());
            }
            cards_reply(monitor, &matches, show_pii)
        }
    }
}

fn status_reply(monitor: &Monitor) -> String {
    let settings = monitor.settings();
    let readings = monitor.poll_all(settings.balance_cache_ttl, settings.on_demand_delay);
    let rows: Vec<StatusRow> = readings
        .iter()
        .map(|(meter, result)| StatusRow {
            label: meter.label().to_string(),
            number: display_number(monitor, meter, result.as_ref().ok()),
            balance: result.as_ref().ok().map(|b| b.balance),
            threshold: meter.threshold.0,
        })
        .collect();
    whatsapp_status(&rows, &synced_now())
}

/// Account number when known, otherwise the meter number.
fn display_number(monitor: &Monitor, meter: &Meter, balance: Option<&Balance>) -> String {
    if let Some(b) = balance {
        if !b.account_no.is_empty() {
            return b.account_no.clone();
        }
    }
    let known = monitor.known(meter);
    known
        .account
        .map(|a| a.to_string())
        .or_else(|| known.meter.map(|m| m.to_string()))
        .unwrap_or_default()
}

fn info_reply(monitor: &Monitor, show_pii: bool) -> String {
    let delay = monitor.settings().on_demand_delay;
    let meters: Vec<&Meter> = monitor.enabled().collect();
    let rows: Vec<InfoRow> = meters
        .iter()
        .enumerate()
        .map(|(i, meter)| {
            let info = monitor.info(meter).ok();
            if i + 1 < meters.len() {
                std::thread::sleep(delay);
            }
            let known = monitor.known(meter);
            InfoRow {
                label: meter.label().to_string(),
                account_no: info
                    .as_ref()
                    .map(|x| x.account_no.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| known.account.map(|a| a.to_string()))
                    .unwrap_or_default(),
                meter_no: info
                    .as_ref()
                    .map(|x| x.meter_no.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| known.meter.map(|m| m.to_string()))
                    .unwrap_or_default(),
                load_kw: info.as_ref().and_then(|x| x.sanction_load_kw),
                customer_name: info.and_then(|x| x.customer_name),
            }
        })
        .collect();
    whatsapp_info(&rows, show_pii)
}

fn cards_reply(monitor: &Monitor, meters: &[&Meter], show_pii: bool) -> String {
    if meters.len() > MAX_CARDS {
        let mut out = format!("{} meters match. Be more specific:", meters.len());
        for meter in meters {
            out.push_str(&format!("\n• {}", meter.unit_name));
        }
        return out;
    }
    let settings = monitor.settings();
    let cards: Vec<String> = meters
        .iter()
        .map(
            |meter| match monitor.balance(meter, settings.balance_cache_ttl) {
                Err(e) => format!(
                    "⚠️ *{}*: could not read the meter right now ({e}).",
                    meter.unit_name
                ),
                Ok(balance) => {
                    let info = monitor.info(meter).ok();
                    whatsapp_card(
                        &CardData {
                            unit_name: &meter.unit_name,
                            threshold: meter.threshold.0,
                            balance: &balance,
                            info: info.as_ref(),
                        },
                        show_pii,
                    )
                }
            },
        )
        .collect();
    cards.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_commands() {
        for word in ["all", "STATUS", " table ", "/list", "balance"] {
            assert_eq!(classify(word), Query::Status, "{word}");
        }
        for word in ["info", "Meters", "directory"] {
            assert_eq!(classify(word), Query::Info, "{word}");
        }
        for word in ["help", "", "hi", "?"] {
            assert_eq!(classify(word), Query::Help, "{word}");
        }
    }

    #[test]
    fn classifies_last3_and_names() {
        assert!(matches!(classify("421"), Query::Last3(l) if l.as_str() == "421"));
        assert!(matches!(classify(" 011 "), Query::Last3(l) if l.as_str() == "011"));
        assert_eq!(classify("1A"), Query::Name("1a".to_string()));
        // Four digits is not a last-3 lookup.
        assert!(matches!(classify("4211"), Query::Name(_)));
    }
}
