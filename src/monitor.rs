//! The service object shared by the CLI, the scheduled check and the webhook server:
//! meter list, API client, caches, identity resolution and the alert run.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::clock;
use crate::config::{Meter, Settings};
use crate::desco_api::{ApiError, Balance, CustomerInfo, DescoClient};
use crate::domain::{AccountNo, Backend, Identifier, Last3, MeterNo};
use crate::formatter::alert_message;
use crate::notifier::Notifier;
use crate::state::{
    AlertState, Decision, Identity, IdentityCache, alert_state_path, identity_cache_path,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What is known about a meter: configuration merged with what the API told us earlier.
#[derive(Debug, Clone, Default)]
pub struct Known {
    pub account: Option<AccountNo>,
    pub meter: Option<MeterNo>,
    pub backend: Option<Backend>,
}

pub struct Monitor {
    api: DescoClient,
    meters: Vec<Meter>,
    settings: Settings,
    identities: Mutex<IdentityCache>,
    balances: Mutex<HashMap<String, (Instant, Balance)>>,
    infos: Mutex<HashMap<String, (Instant, CustomerInfo)>>,
}

#[derive(Debug, Default)]
pub struct CheckReport {
    pub checked: usize,
    pub unreadable: usize,
    pub low: usize,
    pub alerts_sent: usize,
    pub notify_failures: usize,
    /// Alert texts that would be (dry run) or were sent.
    pub messages: Vec<String>,
}

impl Monitor {
    #[must_use]
    pub fn new(api: DescoClient, meters: Vec<Meter>, settings: Settings) -> Self {
        let identities = IdentityCache::load(&identity_cache_path(&settings.state_dir));
        Self {
            api,
            meters,
            settings,
            identities: Mutex::new(identities),
            balances: Mutex::new(HashMap::new()),
            infos: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    #[must_use]
    pub fn api(&self) -> &DescoClient {
        &self.api
    }

    /// Enabled meters in configuration order.
    pub fn enabled(&self) -> impl Iterator<Item = &Meter> {
        self.meters.iter().filter(|m| m.enabled)
    }

    #[must_use]
    pub fn known(&self, meter: &Meter) -> Known {
        let cache = lock(&self.identities);
        let cached = cache.get(&meter.unit_name);
        Known {
            account: meter.account.clone().or_else(|| {
                cached
                    .and_then(|c| c.account_no.as_deref())
                    .and_then(|a| AccountNo::parse(a).ok())
            }),
            meter: meter.meter.clone().or_else(|| {
                cached
                    .and_then(|c| c.meter_no.as_deref())
                    .and_then(|m| MeterNo::parse(m).ok())
            }),
            backend: meter.backend_hint.or_else(|| {
                cached
                    .and_then(|c| c.backend.as_deref())
                    .and_then(|b| Backend::parse(b).ok())
            }),
        }
    }

    fn identifier(&self, meter: &Meter) -> Option<(Identifier, Option<Backend>)> {
        let known = self.known(meter);
        let id = known
            .account
            .map(Identifier::Account)
            .or_else(|| known.meter.map(Identifier::Meter))?;
        Some((id, known.backend))
    }

    fn remember(&self, meter: &Meter, account_no: &str, meter_no: &str, backend: Backend) {
        let identity = Identity {
            account_no: AccountNo::parse(account_no)
                .ok()
                .map(|a| a.as_str().to_string()),
            meter_no: MeterNo::parse(meter_no)
                .ok()
                .map(|m| m.as_str().to_string()),
            backend: Some(backend.as_str().to_string()),
        };
        let mut cache = lock(&self.identities);
        if cache.update(&meter.unit_name, identity) {
            let path = identity_cache_path(&self.settings.state_dir);
            if let Err(e) = cache.save(&path) {
                warn!(error = %e, "could not save identity cache");
            }
        }
    }

    /// Balance for one meter. A cached value younger than `max_age` is reused; the flag says
    /// whether the network was used.
    fn balance_inner(&self, meter: &Meter, max_age: Duration) -> (Result<Balance, ApiError>, bool) {
        if !max_age.is_zero() {
            if let Some((at, balance)) = lock(&self.balances).get(&meter.unit_name) {
                if at.elapsed() <= max_age {
                    return (Ok(balance.clone()), false);
                }
            }
        }
        let Some((id, hint)) = self.identifier(meter) else {
            return (
                Err(ApiError::Malformed("meter has no identifier".to_string())),
                false,
            );
        };
        let result = self.api.balance(&id, hint);
        if let Ok(balance) = &result {
            self.remember(
                meter,
                &balance.account_no,
                &balance.meter_no,
                balance.backend,
            );
            lock(&self.balances).insert(meter.unit_name.clone(), (Instant::now(), balance.clone()));
        }
        (result, true)
    }

    pub fn balance(&self, meter: &Meter, max_age: Duration) -> Result<Balance, ApiError> {
        self.balance_inner(meter, max_age).0
    }

    /// Customer info, cached for `info_cache_ttl` (it rarely changes).
    pub fn info(&self, meter: &Meter) -> Result<CustomerInfo, ApiError> {
        if let Some((at, info)) = lock(&self.infos).get(&meter.unit_name) {
            if at.elapsed() <= self.settings.info_cache_ttl {
                return Ok(info.clone());
            }
        }
        let (id, hint) = self
            .identifier(meter)
            .ok_or_else(|| ApiError::Malformed("meter has no identifier".to_string()))?;
        let info = self.api.customer_info(&id, hint)?;
        self.remember(meter, &info.account_no, &info.meter_no, info.backend);
        lock(&self.infos).insert(meter.unit_name.clone(), (Instant::now(), info.clone()));
        Ok(info)
    }

    /// Reads every enabled meter, pausing `delay` after each real network call.
    pub fn poll_all(
        &self,
        max_age: Duration,
        delay: Duration,
    ) -> Vec<(&Meter, Result<Balance, ApiError>)> {
        let meters: Vec<&Meter> = self.enabled().collect();
        let last = meters.len().saturating_sub(1);
        let mut out = Vec::with_capacity(meters.len());
        for (i, meter) in meters.into_iter().enumerate() {
            let (result, from_network) = self.balance_inner(meter, max_age);
            if from_network && i != last {
                thread::sleep(delay);
            }
            out.push((meter, result));
        }
        out
    }

    /// Meters whose account or meter number ends with `last3`. Fills in missing numbers
    /// from the API first, so a config that lists only one number still matches both.
    pub fn find_by_last3(&self, last3: &Last3) -> Vec<&Meter> {
        self.resolve_missing_identities();
        self.enabled()
            .filter(|m| {
                let known = self.known(m);
                known.account.is_some_and(|a| a.last3() == last3.as_str())
                    || known.meter.is_some_and(|n| n.last3() == last3.as_str())
            })
            .collect()
    }

    fn resolve_missing_identities(&self) {
        let unresolved: Vec<&Meter> = self
            .enabled()
            .filter(|m| {
                let k = self.known(m);
                k.account.is_none() || k.meter.is_none()
            })
            .collect();
        for meter in unresolved {
            let (result, from_network) = self.balance_inner(meter, self.settings.balance_cache_ttl);
            if let Err(e) = result {
                warn!(unit = %meter.unit_name, error = %e, "could not resolve meter identity");
            }
            if from_network {
                thread::sleep(self.settings.on_demand_delay);
            }
        }
    }

    /// Meters matching a unit name: exact label, then whole-word, then substring.
    pub fn find_by_name(&self, text: &str) -> Vec<&Meter> {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let tiers: [&dyn Fn(&str) -> bool; 3] = [
            &|name| name == needle,
            &|name| name.split_whitespace().any(|w| w == needle),
            &|name| name.contains(&needle),
        ];
        for matches in tiers {
            let found: Vec<&Meter> = self
                .enabled()
                .filter(|m| {
                    matches(&m.unit_name.to_lowercase())
                        || m.short_name
                            .as_deref()
                            .is_some_and(|s| matches(&s.to_lowercase()))
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    /// Finds one configured meter by unit name, last 3 digits or full number.
    #[must_use]
    pub fn find_configured(&self, text: &str) -> Option<&Meter> {
        let text = text.trim();
        if let Ok(last3) = Last3::parse(text) {
            if let Some(m) = self.find_by_last3(&last3).first() {
                return Some(m);
            }
        }
        if let Ok(id) = Identifier::parse(text) {
            let found = self.enabled().find(|m| {
                let k = self.known(m);
                match &id {
                    Identifier::Account(a) => k.account.as_ref() == Some(a),
                    Identifier::Meter(n) => k.meter.as_ref() == Some(n),
                }
            });
            if found.is_some() {
                return found;
            }
        }
        self.find_by_name(text).first().copied()
    }

    /// Polls every meter and sends alerts according to the de-duplication rules.
    /// With `dry_run` nothing is sent and no state is written.
    pub fn run_check(&self, notifier: &Notifier, dry_run: bool) -> CheckReport {
        let state_path = alert_state_path(&self.settings.state_dir);
        let mut state = AlertState::load(&state_path);
        let mut report = CheckReport::default();
        let now = clock::format_minute(clock::dhaka_now());

        for (meter, result) in self.poll_all(Duration::ZERO, self.settings.rate_limit) {
            report.checked += 1;
            let balance = match result {
                Ok(b) => b,
                Err(e) => {
                    warn!(unit = %meter.unit_name, error = %e, "could not read meter");
                    report.unreadable += 1;
                    continue;
                }
            };
            let key = &meter.unit_name;
            let decision = state.decide(
                key,
                balance.balance,
                meter.threshold.0,
                &balance.reading_time,
                self.settings.followup_drop,
            );
            info!(unit = %key, balance = balance.balance, ?decision, "evaluated");

            if balance.balance < meter.threshold.0 {
                report.low += 1;
            }
            match decision {
                Decision::Recovered => {
                    if !dry_run {
                        state.clear(key);
                    }
                }
                Decision::Ok | Decision::AlreadyAlerted => {}
                Decision::AlertNew | Decision::AlertFollowUp => {
                    let follow_up = decision == Decision::AlertFollowUp;
                    let previous = state.get(key).map(|e| e.last_alerted_balance);
                    let alert = alert_message(
                        &meter.unit_name,
                        meter.threshold.0,
                        &balance,
                        follow_up,
                        previous,
                    );
                    report.messages.push(alert.text.clone());
                    if dry_run {
                        println!("{}\n", alert.text);
                        continue;
                    }
                    let tenant = self.tenant_phone(meter, notifier);
                    match notifier.send_alert(&alert, tenant.as_deref()) {
                        Ok(()) => {
                            report.alerts_sent += 1;
                            state.record_alert(key, balance.balance, &balance.reading_time, &now);
                        }
                        Err(e) => {
                            // Not recorded, so the next run tries again.
                            warn!(unit = %key, error = %e, "alert delivery failed");
                            report.notify_failures += 1;
                        }
                    }
                }
            }
        }

        if !dry_run {
            if let Err(e) = state.save(&state_path) {
                warn!(error = %e, "could not save alert state");
            }
        }
        report
    }

    /// Phone of the tenant to notify directly: the configured `notify_phone`, or the contact
    /// number DESCO has on file, when `NOTIFY_TENANTS=true`.
    fn tenant_phone(&self, meter: &Meter, notifier: &Notifier) -> Option<String> {
        if !notifier.notify_tenants() {
            return None;
        }
        if let Some(phone) = &meter.notify_phone {
            return Some(phone.clone());
        }
        self.info(meter).ok().and_then(|i| i.contact_no).map(|raw| {
            // Bangladeshi mobile numbers on file look like 01712345678; WhatsApp needs 8801712345678.
            let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
            if digits.starts_with("01") && digits.len() == 11 {
                format!("88{digits}")
            } else {
                digits
            }
        })
    }
}
