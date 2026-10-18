//! Property-based and snapshot tests.

use desco_sentinel::desco_api::Balance;
use desco_sentinel::domain::{AccountNo, Backend, Identifier, Last3, MeterNo};
use desco_sentinel::formatter::{
    MAX_LINE_WIDTH, StatusRow, alert_message, display_width, whatsapp_status,
};
use desco_sentinel::notifier::split_message;
use desco_sentinel::state::AlertState;
use proptest::prelude::*;

proptest! {
    #[test]
    fn parsers_never_panic(s in "\\PC{0,40}") {
        let _ = Identifier::parse(&s);
        let _ = AccountNo::parse(&s);
        let _ = MeterNo::parse(&s);
        let _ = Last3::parse(&s);
    }

    #[test]
    fn digit_strings_classify_by_length(digits in "[0-9]{1,16}") {
        match Identifier::parse(&digits) {
            Ok(Identifier::Meter(m)) => prop_assert_eq!(m.as_str().len(), 12),
            Ok(Identifier::Account(a)) => prop_assert!((6..=11).contains(&a.as_str().len())),
            Err(_) => prop_assert!(!(6..=12).contains(&digits.len())),
        }
    }

    #[test]
    fn desco_prefix_is_ignored(digits in "[0-9]{8,10}") {
        let plain = AccountNo::parse(&digits).expect("valid");
        let prefixed = AccountNo::parse(&format!("DESCO-{digits}")).expect("valid");
        prop_assert_eq!(plain, prefixed);
    }

    /// Whatever the unit names and balances are, no table line may exceed the phone width.
    #[test]
    fn status_table_never_exceeds_phone_width(
        labels in prop::collection::vec("[ -~]{1,30}", 1..25),
        balances in prop::collection::vec(prop::option::of(-100_000.0f64..10_000_000.0), 25),
        numbers in prop::collection::vec("[0-9]{6,12}", 25),
    ) {
        let rows: Vec<StatusRow> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| StatusRow {
                label: label.clone(),
                number: numbers[i].clone(),
                balance: balances[i],
                threshold: 150.0,
            })
            .collect();
        let text = whatsapp_status(&rows, "2026-10-18 10:00");
        let block = text.split("```").nth(1).expect("fenced block");
        for line in block.lines().filter(|l| !l.is_empty()) {
            prop_assert!(display_width(line) <= MAX_LINE_WIDTH, "too wide: '{}'", line);
        }
    }

    /// A balance at or above the threshold can never trigger an alert, whatever the history.
    #[test]
    fn healthy_balance_never_alerts(
        balance in 150.0f64..100_000.0,
        previous in 0.0f64..150.0,
        reading_a in "[a-z0-9]{1,8}",
        reading_b in "[a-z0-9]{1,8}",
    ) {
        let mut state = AlertState::default();
        state.record_alert("m", previous, &reading_a, "t");
        let decision = state.decide("m", balance, 150.0, &reading_b, 25.0);
        prop_assert!(!decision.should_alert());
    }

    /// Same reading again never produces a follow-up, however far the balance fell.
    #[test]
    fn same_reading_never_repeats_an_alert(
        first in 0.0f64..149.0,
        second in 0.0f64..149.0,
        reading in "[a-z0-9]{1,8}",
    ) {
        let mut state = AlertState::default();
        state.record_alert("m", first, &reading, "t");
        prop_assert!(!state.decide("m", second, 150.0, &reading, 25.0).should_alert());
    }

    #[test]
    fn split_message_never_loses_text(text in "[a-z \\n]{0,400}", max in 5usize..60) {
        let parts = split_message(&text, max);
        prop_assert!(parts.iter().all(|p| p.chars().count() <= max));
        let rejoined: String = parts.join("\n");
        let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        prop_assert_eq!(squash(&rejoined), squash(&text));
    }
}

fn sample_balance(amount: f64) -> Balance {
    Balance {
        account_no: "98765421".to_string(),
        meter_no: "667788990011".to_string(),
        balance: amount,
        month_consumption: Some(2020.49),
        reading_time: "2026-10-18".to_string(),
        backend: Backend::Unified,
    }
}

#[test]
fn snapshot_status_table() {
    let rows = vec![
        StatusRow {
            label: "Apt 1A".into(),
            number: "98765421".into(),
            balance: Some(1384.35),
            threshold: 150.0,
        },
        StatusRow {
            label: "Apt 1B".into(),
            number: "98765812".into(),
            balance: Some(95.5),
            threshold: 150.0,
        },
        StatusRow {
            label: "Apt 2A".into(),
            number: "98765309".into(),
            balance: Some(450.0),
            threshold: 150.0,
        },
        StatusRow {
            label: "Lift".into(),
            number: "98765999".into(),
            balance: Some(620.0),
            threshold: 500.0,
        },
        StatusRow {
            label: "Pump".into(),
            number: "98765998".into(),
            balance: None,
            threshold: 500.0,
        },
    ];
    insta::assert_snapshot!(whatsapp_status(&rows, "2026-10-18 00:00"));
}

#[test]
fn snapshot_alert_message() {
    let alert = alert_message("Apt 1B", 150.0, &sample_balance(95.5), false, None);
    insta::assert_snapshot!(alert.text);
}

#[test]
fn snapshot_follow_up_alert_message() {
    let alert = alert_message("Apt 1B", 150.0, &sample_balance(60.0), true, Some(95.5));
    insta::assert_snapshot!(alert.text);
}
