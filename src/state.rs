//! Persistent state: alert de-duplication and the resolved account/meter/backend cache.
//!
//! Alert rules (per meter):
//! * balance >= threshold: forget the meter (so the next drop alerts again).
//! * balance < threshold and never alerted: alert.
//! * already alerted: alert again only if the reading changed AND the balance fell by at least
//!   `followup_drop` BDT since the last alert.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Balance is fine and nothing was recorded.
    Ok,
    /// Balance is fine and a previous low-balance record should be removed.
    Recovered,
    /// Low, but already reported for this situation.
    AlreadyAlerted,
    /// Newly low.
    AlertNew,
    /// Still low and dropped further since the last alert.
    AlertFollowUp,
}

impl Decision {
    #[must_use]
    pub fn should_alert(&self) -> bool {
        matches!(self, Self::AlertNew | Self::AlertFollowUp)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertEntry {
    pub last_alerted_balance: f64,
    pub last_reading_time: String,
    pub alerted_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AlertState {
    entries: BTreeMap<String, AlertEntry>,
}

impl AlertState {
    /// Pure decision, no mutation. Apply it with [`AlertState::record_alert`] after the
    /// notification was delivered, or [`AlertState::clear`] on `Recovered`.
    #[must_use]
    pub fn decide(
        &self,
        key: &str,
        balance: f64,
        threshold: f64,
        reading_time: &str,
        followup_drop: f64,
    ) -> Decision {
        let previous = self.entries.get(key);
        if balance >= threshold {
            return if previous.is_some() {
                Decision::Recovered
            } else {
                Decision::Ok
            };
        }
        match previous {
            None => Decision::AlertNew,
            Some(entry) => {
                let reading_changed = entry.last_reading_time != reading_time;
                let dropped = entry.last_alerted_balance - balance;
                if reading_changed && dropped >= followup_drop {
                    Decision::AlertFollowUp
                } else {
                    Decision::AlreadyAlerted
                }
            }
        }
    }

    pub fn record_alert(&mut self, key: &str, balance: f64, reading_time: &str, now: &str) {
        self.entries.insert(
            key.to_string(),
            AlertEntry {
                last_alerted_balance: balance,
                last_reading_time: reading_time.to_string(),
                alerted_at: now.to_string(),
            },
        );
    }

    pub fn clear(&mut self, key: &str) {
        self.entries.remove(key);
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&AlertEntry> {
        self.entries.get(key)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Missing or unreadable state starts fresh (worst case: one repeated alert).
    #[must_use]
    pub fn load(path: &Path) -> Self {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_json(path, self)
    }
}

/// Account/meter numbers and backend discovered from API answers, so that last-3-digit lookups
/// work even when `meters.json` lists only one of the two numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub account_no: Option<String>,
    pub meter_no: Option<String>,
    pub backend: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IdentityCache {
    entries: BTreeMap<String, Identity>,
}

impl IdentityCache {
    #[must_use]
    pub fn load(path: &Path) -> Self {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_json(path, self)
    }

    #[must_use]
    pub fn get(&self, unit: &str) -> Option<&Identity> {
        self.entries.get(unit)
    }

    /// Returns `true` when the stored value changed.
    pub fn update(&mut self, unit: &str, identity: Identity) -> bool {
        if self.entries.get(unit) == Some(&identity) {
            return false;
        }
        self.entries.insert(unit.to_string(), identity);
        true
    }
}

#[must_use]
pub fn alert_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("alert_state.json")
}

#[must_use]
pub fn identity_cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join("identity_cache.json")
}

fn load_json<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> T {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            warn!(path = %path.display(), error = %e, "state file unreadable, starting fresh");
            T::default()
        }),
        Err(_) => T::default(),
    }
}

/// Write to a temp file and rename, so a crash never leaves half a JSON file behind.
fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_drop_then_dedup_then_followup_then_recovery() {
        let mut state = AlertState::default();
        let t = 150.0;

        assert_eq!(state.decide("A", 400.0, t, "d1", 25.0), Decision::Ok);
        assert_eq!(state.decide("A", 140.0, t, "d1", 25.0), Decision::AlertNew);

        state.record_alert("A", 140.0, "d1", "now");
        // Same reading, same balance: no spam.
        assert_eq!(
            state.decide("A", 140.0, t, "d1", 25.0),
            Decision::AlreadyAlerted
        );
        // New reading but only a small drop: still quiet.
        assert_eq!(
            state.decide("A", 130.0, t, "d2", 25.0),
            Decision::AlreadyAlerted
        );
        // Same reading, big drop: still quiet (reading did not change).
        assert_eq!(
            state.decide("A", 100.0, t, "d1", 25.0),
            Decision::AlreadyAlerted
        );
        // New reading and drop of exactly 25: follow-up.
        assert_eq!(
            state.decide("A", 115.0, t, "d2", 25.0),
            Decision::AlertFollowUp
        );

        // Recharge clears the record.
        assert_eq!(state.decide("A", 900.0, t, "d3", 25.0), Decision::Recovered);
        state.clear("A");
        assert_eq!(state.decide("A", 120.0, t, "d4", 25.0), Decision::AlertNew);
    }

    #[test]
    fn threshold_is_exclusive() {
        let state = AlertState::default();
        assert_eq!(state.decide("A", 150.0, 150.0, "d", 25.0), Decision::Ok);
        assert_eq!(
            state.decide("A", 149.99, 150.0, "d", 25.0),
            Decision::AlertNew
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("desco-state-test-{}", std::process::id()));
        let path = alert_state_path(&dir);
        let mut state = AlertState::default();
        state.record_alert("Apt 1A", 99.5, "2026-10-18", "2026-10-18 10:00");
        state.save(&path).expect("save works");
        let loaded = AlertState::load(&path);
        assert_eq!(
            loaded.get("Apt 1A").map(|e| e.last_alerted_balance),
            Some(99.5)
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_state_starts_fresh() {
        let dir = std::env::temp_dir().join(format!("desco-corrupt-test-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = alert_state_path(&dir);
        fs::write(&path, "{not json").expect("write");
        assert!(AlertState::load(&path).is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
