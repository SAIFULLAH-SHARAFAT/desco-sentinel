//! Offline tests: DESCO payload parsing, error handling and last-3-digit matching.
//! The fixtures follow real API responses but contain only made-up numbers and names.

use std::time::Duration;

use desco_sentinel::config::{Env, Settings, parse_meters};
use desco_sentinel::desco_api::{
    ApiError, DescoClient, Rejection, extract_data, parse_balance, parse_customer_info,
    parse_monthly, parse_recharges,
};
use desco_sentinel::domain::{Backend, Bdt, Last3};
use desco_sentinel::monitor::Monitor;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn unified_balance_parses() {
    let body = fixture("unified_balance_ok.json");
    let data = extract_data(&body).expect("success envelope");
    let balance = parse_balance(data, Backend::Unified).expect("balance parses");
    assert_eq!(balance.account_no, "98765421");
    assert_eq!(balance.meter_no, "667788990011");
    assert!((balance.balance - 1384.35).abs() < 1e-9);
    assert_eq!(balance.month_consumption, Some(2020.49));
    // Real readings are often a bare date; it must stay an opaque string.
    assert_eq!(balance.reading_time, "2026-10-18");
    assert_eq!(balance.backend, Backend::Unified);
}

#[test]
fn tkdes_balance_accepts_numbers_as_strings_and_datetime_readings() {
    let body = fixture("tkdes_balance_string_numbers.json");
    let data = extract_data(&body).expect("success envelope");
    let balance = parse_balance(data, Backend::Tkdes).expect("balance parses");
    assert!((balance.balance - 95.5).abs() < 1e-9);
    assert_eq!(balance.month_consumption, None);
    assert_eq!(balance.reading_time, "2026-10-18 00:00:00");
}

#[test]
fn declined_answers_are_rejections_not_data() {
    // unified answers HTTP 200 with null data when the meter lives on tkdes.
    let null_data = fixture("unified_balance_null.json");
    assert_eq!(
        extract_data(&null_data),
        Err(Rejection {
            code: 200,
            desc: "OK".to_string()
        })
    );
    // The error codes differ per endpoint and identifier type.
    let account = fixture("tkdes_account_not_found_16001.json");
    assert_eq!(extract_data(&account).expect_err("16001").code, 16001);
    let meter = fixture("meter_not_found_16002.json");
    assert_eq!(extract_data(&meter).expect_err("16002").code, 16002);
    // Garbage is also a rejection, never a panic.
    assert!(extract_data(&serde_json::json!({})).is_err());
    assert!(extract_data(&serde_json::json!([1, 2])).is_err());
}

#[test]
fn balance_without_a_number_is_malformed() {
    let data = serde_json::json!({ "accountNo": "98765421" });
    assert!(matches!(
        parse_balance(&data, Backend::Unified),
        Err(ApiError::Malformed(_))
    ));
}

#[test]
fn customer_info_trims_blank_contact_and_reads_load() {
    let body = fixture("customer_info_ok.json");
    let info = parse_customer_info(extract_data(&body).expect("ok"), Backend::Unified);
    assert_eq!(info.customer_name.as_deref(), Some("MR. JOHN DOE"));
    assert_eq!(
        info.contact_no, None,
        "a single space is not a phone number"
    );
    assert_eq!(info.sanction_load_kw, Some(6.0));
    assert_eq!(info.phase_type.as_deref(), Some("1P"));
}

#[test]
fn recharges_are_newest_first_and_dates_are_cleaned() {
    let body = fixture("recharge_history.json");
    let recharges = parse_recharges(extract_data(&body).expect("ok")).expect("list");
    assert_eq!(recharges.len(), 2);
    assert_eq!(recharges[0].date, "2026-10-06 16:50:23");
    assert_eq!(recharges[1].date, "2026-09-19 18:49:01");
    assert!((recharges[0].total - 1000.0).abs() < 1e-9);
}

#[test]
fn monthly_is_newest_first_and_empty_lists_are_fine() {
    let body = fixture("monthly.json");
    let months = parse_monthly(extract_data(&body).expect("ok")).expect("list");
    assert_eq!(months[0].month, "2026-08");
    assert_eq!(months[1].month, "2026-07");
    assert_eq!(months[1].max_demand, Some(0.482));

    let empty = fixture("empty_list.json");
    let data = extract_data(&empty).expect("an empty list is still data");
    assert_eq!(parse_monthly(data).expect("list"), Vec::new());
}

// ---- last-3-digit lookup ------------------------------------------------------------------

/// A monitor that never touches the network: every meter lists both numbers, and the API
/// points at a closed local port.
fn offline_monitor() -> Monitor {
    let json = r#"[
        {"unit_name": "Apt 1A", "account_no": "98765421", "meter_no": "667788990011"},
        {"unit_name": "Apt 1B", "account_no": "98765812", "meter_no": "667788990421"},
        {"unit_name": "Apt 2A", "account_no": "98765309", "meter_no": "667788990310"},
        {"unit_name": "Apt 11A", "account_no": "98765555", "meter_no": "667788990777"},
        {"unit_name": "Lift", "short_name": "Lift", "account_no": "98765999", "meter_no": "667788990099", "threshold_bdt": 500},
        {"unit_name": "Disabled", "account_no": "98765000", "meter_no": "667788990000", "enabled": false}
    ]"#;
    let meters = parse_meters(json, Bdt(150.0)).expect("meters parse");
    let state_dir = std::env::temp_dir().join(format!("desco-offline-{}", std::process::id()));
    let state_dir = state_dir.to_string_lossy().to_string();
    let env = Env::from_pairs([("STATE_DIR", state_dir.as_str())]);
    let settings = Settings::from_env(&env).expect("settings");
    let api =
        DescoClient::new("http://127.0.0.1:9", Duration::from_secs(1), 0, false).expect("client");
    Monitor::new(api, meters, settings)
}

fn names(found: &[&desco_sentinel::config::Meter]) -> Vec<String> {
    found.iter().map(|m| m.unit_name.clone()).collect()
}

#[test]
fn last3_matches_account_suffix() {
    let monitor = offline_monitor();
    let found = monitor.find_by_last3(&Last3::parse("421").expect("3 digits"));
    // 421 is the end of Apt 1A's account (98765421) AND of Apt 1B's meter (…990421).
    assert_eq!(names(&found), vec!["Apt 1A", "Apt 1B"]);
}

#[test]
fn last3_matches_meter_suffix() {
    let monitor = offline_monitor();
    let found = monitor.find_by_last3(&Last3::parse("011").expect("3 digits"));
    assert_eq!(names(&found), vec!["Apt 1A"]);
}

#[test]
fn last3_ignores_disabled_and_unknown() {
    let monitor = offline_monitor();
    assert!(
        monitor
            .find_by_last3(&Last3::parse("000").expect("3 digits"))
            .is_empty()
    );
    assert!(
        monitor
            .find_by_last3(&Last3::parse("123").expect("3 digits"))
            .is_empty()
    );
}

#[test]
fn unit_names_match_exact_then_word_then_substring() {
    let monitor = offline_monitor();
    assert_eq!(
        names(&monitor.find_by_name("1A")),
        vec!["Apt 1A"],
        "whole word beats 'Apt 11A'"
    );
    assert_eq!(
        names(&monitor.find_by_name("apt 1a")),
        vec!["Apt 1A"],
        "exact, case-insensitive"
    );
    assert_eq!(
        names(&monitor.find_by_name("lif")),
        vec!["Lift"],
        "substring"
    );
    assert_eq!(names(&monitor.find_by_name("11a")), vec!["Apt 11A"]);
    assert!(monitor.find_by_name("nothing").is_empty());
    assert!(monitor.find_by_name("  ").is_empty());
}

#[test]
fn example_config_has_20_valid_meters() {
    let path = format!("{}/config/meters.example.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).expect("example config exists");
    let meters = parse_meters(&text, Bdt(150.0)).expect("example config is valid");
    assert_eq!(meters.len(), 20);
    assert!(
        meters.iter().any(|m| m.account.is_none()),
        "shows a meter-only entry"
    );
    assert!(
        meters.iter().any(|m| m.meter.is_none()),
        "shows an account-only entry"
    );
    assert!(
        meters
            .iter()
            .any(|m| (m.threshold.0 - 500.0).abs() < f64::EPSILON)
    );
}
