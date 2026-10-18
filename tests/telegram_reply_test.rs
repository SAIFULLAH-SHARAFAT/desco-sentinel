//! Who gets answered on Telegram, tested without any network access.

use std::time::Duration;

use desco_sentinel::config::{Env, Settings, parse_meters};
use desco_sentinel::desco_api::DescoClient;
use desco_sentinel::domain::Bdt;
use desco_sentinel::monitor::Monitor;
use desco_sentinel::telegram_bot::compose_reply;

fn offline_monitor() -> Monitor {
    let json = r#"[{"unit_name": "Apt 1A", "account_no": "98765421", "meter_no": "667788990011"}]"#;
    let meters = parse_meters(json, Bdt(150.0)).expect("meters parse");
    let dir = std::env::temp_dir().join(format!("desco-tg-{}", std::process::id()));
    let dir = dir.to_string_lossy().to_string();
    let settings =
        Settings::from_env(&Env::from_pairs([("STATE_DIR", dir.as_str())])).expect("settings");
    let api =
        DescoClient::new("http://127.0.0.1:9", Duration::from_secs(1), 0, false).expect("client");
    Monitor::new(api, meters, settings)
}

#[test]
fn allowed_chat_gets_an_answer() {
    let allowed = vec!["111".to_string(), "-222".to_string()];
    let reply = compose_reply(&offline_monitor(), &allowed, "-222", "help");
    assert!(reply.contains("commands"), "{reply}");
}

#[test]
fn stranger_only_learns_their_own_chat_id() {
    let allowed = vec!["111".to_string()];
    let reply = compose_reply(&offline_monitor(), &allowed, "999", "status");
    assert!(reply.contains("private"));
    assert!(reply.contains("`999`"));
    assert!(!reply.contains("98765421") && !reply.contains("Apt 1A"));
}

#[test]
fn empty_allow_list_answers_nobody() {
    let reply = compose_reply(&offline_monitor(), &[], "111", "help");
    assert!(reply.contains("private"));
}
