//! Text rendering for WhatsApp / Telegram / terminal. Pure functions only.
//!
//! WhatsApp wraps long lines, so every table line inside the ``` block is kept at or below
//! [`MAX_LINE_WIDTH`] display columns (emoji count as two).

use crate::desco_api::{Balance, CustomerInfo, MonthUsage, Recharge};
use crate::domain::Bdt;

pub const MAX_LINE_WIDTH: usize = 34;

const FENCE: &str = "```";
const RULE: &str = "━━━━━━━━━━━━━━━━━━━━";
const ICON_OK: &str = "🟢";
const ICON_LOW: &str = "🔴";
const ICON_UNKNOWN: &str = "⚪";

/// Approximate terminal/phone display width: emoji are two columns wide.
#[must_use]
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if (c as u32) >= 0x1F000 || c == '⚪' {
                2
            } else {
                1
            }
        })
        .sum()
}

/// Truncates to `width` display columns (never splitting a wide character), then pads.
fn pad_right(s: &str, width: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let mut next = out.clone();
        next.push(c);
        if display_width(&next) > width {
            break;
        }
        out = next;
    }
    while display_width(&out) < width {
        out.push(' ');
    }
    out
}

fn pad_left(s: &str, width: usize) -> String {
    let len = display_width(s);
    if len >= width {
        return s.to_string();
    }
    format!("{}{s}", " ".repeat(width - len))
}

fn tail3(s: &str) -> String {
    let start = s.len().saturating_sub(3);
    format!("..{}", &s[start..])
}

/// One row of the building status table.
#[derive(Debug, Clone)]
pub struct StatusRow {
    pub label: String,
    /// Account number, or meter number when the account is unknown.
    pub number: String,
    /// `None` when the meter could not be read.
    pub balance: Option<f64>,
    pub threshold: f64,
}

impl StatusRow {
    #[must_use]
    pub fn is_low(&self) -> bool {
        self.balance.is_some_and(|b| b < self.threshold)
    }

    fn icon(&self) -> &'static str {
        match self.balance {
            None => ICON_UNKNOWN,
            Some(b) if b < self.threshold => ICON_LOW,
            Some(_) => ICON_OK,
        }
    }
}

/// Mobile-friendly status table for `all` / `status` / `table`.
#[must_use]
pub fn whatsapp_status(rows: &[StatusRow], synced: &str) -> String {
    let low = rows.iter().filter(|r| r.is_low()).count();
    let unread = rows.iter().filter(|r| r.balance.is_none()).count();

    let mut out = format!("⚡ *DESCO BUILDING STATUS ({})* ⚡\n{FENCE}\n", rows.len());
    out.push_str(&format!(
        "{} {} {} St\n",
        pad_right("Unit", 9),
        pad_right("Acc#", 5),
        pad_left("Balance", 9)
    ));
    for row in rows {
        let balance = row
            .balance
            .map_or_else(|| "n/a".to_string(), |b| Bdt(b).to_string());
        out.push_str(&format!(
            "{} {} {} {}\n",
            pad_right(&row.label, 9),
            pad_right(&tail3(&row.number), 5),
            pad_left(&balance, 9),
            row.icon()
        ));
    }
    out.push_str(FENCE);
    out.push('\n');
    let summary_icon = if low > 0 { ICON_LOW } else { ICON_OK };
    out.push_str(&format!(
        "{summary_icon} Low balance: {low} of {}",
        rows.len()
    ));
    if unread > 0 {
        out.push_str(&format!("\n{ICON_UNKNOWN} Could not read: {unread}"));
    }
    out.push_str(&format!(
        "\n\n🕒 _Synced: {synced}_\n💡 _Reply with last 3 digits (e.g. 421) for full meter details._"
    ));
    out
}

/// One row of the info directory.
#[derive(Debug, Clone)]
pub struct InfoRow {
    pub label: String,
    pub account_no: String,
    pub meter_no: String,
    pub load_kw: Option<f64>,
    pub customer_name: Option<String>,
}

/// Directory of all meters for `info` / `meters` / `directory`. Customer names are only
/// included when `show_pii` is set.
#[must_use]
pub fn whatsapp_info(rows: &[InfoRow], show_pii: bool) -> String {
    let mut out = format!("📋 *METER DIRECTORY ({})*\n{FENCE}\n", rows.len());
    out.push_str(&format!(
        "{} {} {} {}\n",
        pad_right("Unit", 9),
        pad_right("Acc#", 5),
        pad_right("Mtr#", 5),
        pad_left("kW", 5)
    ));
    for row in rows {
        let load = row
            .load_kw
            .map_or_else(|| "-".to_string(), |l| format!("{l}"));
        out.push_str(&format!(
            "{} {} {} {}\n",
            pad_right(&row.label, 9),
            pad_right(&tail3(&row.account_no), 5),
            pad_right(&tail3(&row.meter_no), 5),
            pad_left(&load, 5)
        ));
    }
    out.push_str(FENCE);
    if show_pii {
        out.push_str("\n*Customers*");
        for row in rows {
            let name = row.customer_name.as_deref().unwrap_or("-");
            out.push_str(&format!("\n• {}: {name}", row.label));
        }
    } else {
        out.push_str("\n_Customer details are hidden (sender is not on the allow-list)._");
    }
    out
}

/// Everything shown on the detail card.
pub struct CardData<'a> {
    pub unit_name: &'a str,
    pub threshold: f64,
    pub balance: &'a Balance,
    pub info: Option<&'a CustomerInfo>,
}

/// Rich per-meter card for last-3-digit and unit-name lookups.
#[must_use]
pub fn whatsapp_card(card: &CardData<'_>, show_pii: bool) -> String {
    let b = card.balance;
    let low = b.balance < card.threshold;
    let icon = if low { ICON_LOW } else { ICON_OK };
    let mut out = format!(
        "🔌 *{} — METER DETAILS*\n{RULE}\n{icon} *Balance:* *{} BDT*\n",
        card.unit_name.to_uppercase(),
        Bdt(b.balance)
    );
    if let Some(usage) = b.month_consumption {
        out.push_str(&format!("📉 *Month Usage:* {} BDT\n", Bdt(usage)));
    }
    out.push_str(&format!(
        "⚠️ *Alert Threshold:* {} BDT\n",
        Bdt(card.threshold)
    ));
    out.push_str(&format!("🕒 *Reading Time:* {}\n", b.reading_time));

    out.push_str("\n📋 *Account & Hardware Info*\n");
    out.push_str(&format!(
        "• *Account No:* `{}` ({})\n• *Meter No:* `{}` ({})\n",
        b.account_no,
        tail3(&b.account_no),
        b.meter_no,
        tail3(&b.meter_no)
    ));
    if let Some(info) = card.info {
        if show_pii {
            if let Some(name) = &info.customer_name {
                out.push_str(&format!("• *Customer:* {name}\n"));
            }
            if let Some(phone) = &info.contact_no {
                out.push_str(&format!("• *Contact:* {phone}\n"));
            }
        }
        let load = info.sanction_load_kw.map(|l| format!("{l} kW"));
        let phase = info.phase_type.clone();
        let load_phase = match (load, phase) {
            (Some(l), Some(p)) => Some(format!("{l} ({p})")),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        };
        if let Some(text) = load_phase {
            out.push_str(&format!("• *Load / Phase:* {text}\n"));
        }
        if let Some(tariff) = &info.tariff {
            out.push_str(&format!("• *Tariff:* {tariff}\n"));
        }
    }
    out.push_str(&format!("• *Backend:* `{}`", b.backend));
    out
}

/// A rendered low-balance alert plus the fields used by templated channels.
#[derive(Debug, Clone)]
pub struct Alert {
    pub text: String,
    pub unit: String,
    pub balance: String,
    pub threshold: String,
    /// Single line, no newlines (WhatsApp template parameters cannot contain them).
    pub summary: String,
}

#[must_use]
pub fn alert_message(
    unit_name: &str,
    threshold: f64,
    balance: &Balance,
    follow_up: bool,
    previous_alert_balance: Option<f64>,
) -> Alert {
    let title = if follow_up {
        "BALANCE DROPPED FURTHER"
    } else {
        "LOW BALANCE"
    };
    let balance_text = Bdt(balance.balance).to_string();
    let threshold_text = Bdt(threshold).to_string();

    let mut text = format!(
        "{ICON_LOW} *{title} — {}*\n{RULE}\n💰 *Balance:* *{balance_text} BDT* (alert below {threshold_text})\n",
        unit_name.to_uppercase()
    );
    if let (true, Some(prev)) = (follow_up, previous_alert_balance) {
        text.push_str(&format!("🔻 *Last alert at:* {} BDT\n", Bdt(prev)));
    }
    if let Some(usage) = balance.month_consumption {
        text.push_str(&format!("📉 *Month Usage:* {} BDT\n", Bdt(usage)));
    }
    text.push_str(&format!(
        "🕒 *Reading Time:* {}\n• *Account:* `{}`\n• *Meter:* `{}`\n\n⏳ Please recharge soon to avoid the relay cutting power.",
        balance.reading_time, balance.account_no, balance.meter_no
    ));

    let summary = format!(
        "Acc {} | Meter {} | Reading {}",
        balance.account_no, balance.meter_no, balance.reading_time
    );
    Alert {
        text,
        unit: unit_name.to_string(),
        balance: balance_text,
        threshold: threshold_text,
        summary,
    }
}

#[must_use]
pub fn help_text() -> String {
    [
        "🤖 *DESCO Sentinel — commands*",
        "",
        "📊 `status` — balance of every meter (🔴 = below its alert level)",
        "🔎 `825` — details of one meter: type the last 3 digits of its account or meter number",
        "🔎 `status 825` — same as above",
        "🏷 `lift` — details by unit name (part of the name is enough)",
        "📋 `info` — directory of all meters (account, meter, load)",
        "❓ `help` — this message",
        "",
        "Low-balance alerts arrive here automatically. You do not need to ask for them.",
    ]
    .join("\n")
}

/// Plain ASCII table for terminals.
#[must_use]
pub fn ascii_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(columns) {
            widths[i] = widths[i].max(display_width(cell));
        }
    }
    let line = |cells: &[String]| -> String {
        let parts: Vec<String> = cells
            .iter()
            .enumerate()
            .take(columns)
            .map(|(i, c)| pad_right_wide(c, widths[i]))
            .collect();
        format!("| {} |", parts.join(" | "))
    };
    let separator = format!(
        "+{}+",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    );
    let header_cells: Vec<String> = headers.iter().map(ToString::to_string).collect();
    let mut out = vec![separator.clone(), line(&header_cells), separator.clone()];
    out.extend(rows.iter().map(|r| line(r)));
    out.push(separator);
    out.join("\n")
}

/// Like `pad_right` but never truncates.
fn pad_right_wide(s: &str, width: usize) -> String {
    let len = display_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(len)))
}

#[must_use]
pub fn recharge_table(recharges: &[Recharge]) -> String {
    let rows: Vec<Vec<String>> = recharges
        .iter()
        .map(|r| {
            vec![
                r.date.clone(),
                Bdt(r.total).to_string(),
                r.energy
                    .map_or_else(|| "-".to_string(), |e| Bdt(e).to_string()),
                r.operator.clone().unwrap_or_else(|| "-".to_string()),
                r.status.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    ascii_table(
        &["Date", "Total BDT", "Energy BDT", "Operator", "Status"],
        &rows,
    )
}

#[must_use]
pub fn monthly_table(months: &[MonthUsage]) -> String {
    let rows: Vec<Vec<String>> = months
        .iter()
        .map(|m| {
            vec![
                m.month.clone(),
                format!("{:.2}", m.units_kwh),
                Bdt(m.taka).to_string(),
                m.max_demand
                    .map_or_else(|| "-".to_string(), |d| format!("{d:.3}")),
            ]
        })
        .collect();
    ascii_table(&["Month", "kWh", "Cost BDT", "Max kW"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Backend;

    fn balance(amount: f64) -> Balance {
        Balance {
            account_no: "98765421".to_string(),
            meter_no: "667788990011".to_string(),
            balance: amount,
            month_consumption: Some(2020.49),
            reading_time: "2026-10-18".to_string(),
            backend: Backend::Unified,
        }
    }

    fn row(label: &str, number: &str, balance: Option<f64>) -> StatusRow {
        StatusRow {
            label: label.to_string(),
            number: number.to_string(),
            balance,
            threshold: 150.0,
        }
    }

    #[test]
    fn status_table_lines_fit_a_phone() {
        let rows = vec![
            row("Apt 1A", "98765421", Some(1384.35)),
            row("Apt 1B", "98765812", Some(95.5)),
            row("Common - Lift & Pump", "98765999", Some(123_456.78)),
            row("Broken", "98765000", None),
        ];
        let text = whatsapp_status(&rows, "2026-10-18 10:00");
        // Only lines inside the fence must respect the strict limit.
        let inside: Vec<&str> = text
            .split("```")
            .nth(1)
            .expect("fenced block")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(inside.len(), rows.len() + 1);
        for line in inside {
            assert!(display_width(line) <= MAX_LINE_WIDTH, "too wide: '{line}'");
        }
        assert!(text.contains("Low balance: 1 of 4"));
        assert!(text.contains("Could not read: 1"));
    }

    #[test]
    fn emoji_in_unit_names_cannot_overflow_the_table() {
        let rows = vec![row("🏢🏢🏢🏢🏢🏢🏢🏢🏢🏢", "98765421", Some(10.0))];
        let text = whatsapp_status(&rows, "now");
        for line in text.split("```").nth(1).expect("fenced").lines() {
            assert!(display_width(line) <= MAX_LINE_WIDTH, "too wide: '{line}'");
        }
    }

    #[test]
    fn info_table_fits_and_hides_pii() {
        let rows = vec![InfoRow {
            label: "Apt 1A".to_string(),
            account_no: "98765421".to_string(),
            meter_no: "667788990011".to_string(),
            load_kw: Some(2.0),
            customer_name: Some("MR. JOHN DOE".to_string()),
        }];
        let hidden = whatsapp_info(&rows, false);
        assert!(!hidden.contains("JOHN"));
        let shown = whatsapp_info(&rows, true);
        assert!(shown.contains("JOHN"));
        for line in shown.split("```").nth(1).expect("fenced").lines() {
            assert!(display_width(line) <= MAX_LINE_WIDTH);
        }
    }

    #[test]
    fn card_has_both_numbers_and_respects_pii() {
        let b = balance(1384.35);
        let info = CustomerInfo {
            account_no: b.account_no.clone(),
            meter_no: b.meter_no.clone(),
            customer_name: Some("MR. JOHN DOE".to_string()),
            contact_no: Some("01833000000".to_string()),
            sanction_load_kw: Some(6.0),
            phase_type: Some("Single Phase".to_string()),
            tariff: None,
            feeder: None,
            backend: Backend::Unified,
        };
        let card = CardData {
            unit_name: "Apt 1A",
            threshold: 150.0,
            balance: &b,
            info: Some(&info),
        };
        let public = whatsapp_card(&card, false);
        assert!(public.contains("1,384.35 BDT"));
        assert!(public.contains("(..421)") && public.contains("(..011)"));
        assert!(public.contains("6 kW (Single Phase)"));
        assert!(!public.contains("JOHN") && !public.contains("01833"));
        let private = whatsapp_card(&card, true);
        assert!(private.contains("JOHN") && private.contains("01833000000"));
    }

    #[test]
    fn alert_contains_key_facts_and_summary_is_one_line() {
        let b = balance(95.5);
        let alert = alert_message("Apt 1B", 150.0, &b, false, None);
        assert!(alert.text.contains("LOW BALANCE — APT 1B"));
        assert!(alert.text.contains("95.50 BDT"));
        assert!(alert.text.contains("98765421"));
        assert!(!alert.summary.contains('\n'));
        let follow = alert_message("Apt 1B", 150.0, &b, true, Some(140.0));
        assert!(follow.text.contains("DROPPED FURTHER"));
        assert!(follow.text.contains("140.00"));
    }

    #[test]
    fn ascii_table_aligns_columns() {
        let table = ascii_table(&["A", "Bee"], &[vec!["xx".to_string(), "y".to_string()]]);
        let widths: Vec<usize> = table.lines().map(display_width).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]));
    }
}
