//! DESCO prepaid API client.
//!
//! Since 6 Nov 2026 the portal needs a customer sign-in. With credentials configured
//! (`DESCO_USERNAME` / `DESCO_PASSWORD`) the client works against the signed-in portal API:
//! `POST /auth/login` (cookie session plus XSRF header), `GET /accounts` to learn the
//! internal account ids, then `/accounts/{id}/balance|summary|transactions|usage/monthly`.
//! Only accounts linked to that portal login can be read. The session is renewed
//! automatically when it expires.
//!
//! Without credentials the old public endpoints are used (they now answer 401).
//!
//! Behaviour of the old public API, verified against the live service:
//! * Two backends (`unified`, `tkdes`). A meter normally exists on only one.
//! * "Not found" comes back as HTTP 200 with `data: null` (unified getBalance) or with
//!   codes such as 16001 / 16002, so the fallback rule is "code != 200 or data is null".
//! * A 12 digit identifier is a `meterNo`; anything shorter is an `accountNo`.
//!   Sending both only works when they belong together, so we send exactly one.
//! * `readingTime` may be a plain date (`2026-10-18`), so it is kept as an opaque string.

use std::error::Error as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use reqwest::blocking::{Client, RequestBuilder};
use reqwest::cookie::{CookieStore, Jar};
use reqwest::{Method, Url};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{debug, warn};

use crate::domain::{AccountNo, Backend, Identifier};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {0} from DESCO")]
    Http(u16),
    #[error("DESCO requires sign-in (HTTP {0})")]
    AuthRequired(u16),
    #[error("DESCO sign-in failed: {0}")]
    LoginFailed(String),
    #[error("unexpected response: {0}")]
    Malformed(String),
    #[error("meter not found on any backend: {desc} (code {code})")]
    NotFound { code: i64, desc: String },
}

impl ApiError {
    /// One plain sentence for people, used in chat messages.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::AuthRequired(_) => "DESCO's customer portal asks for a sign-in. Set DESCO_USERNAME and DESCO_PASSWORD so balances can be read.".to_string(),
            Self::LoginFailed(why) => format!("Could not sign in to the DESCO portal: {why}."),
            Self::Network(_) => "DESCO's server could not be reached.".to_string(),
            Self::Http(code) => format!("DESCO's server answered with an error (HTTP {code})."),
            Self::Malformed(_) => "DESCO's answer was not in the expected format.".to_string(),
            Self::NotFound { .. } => "DESCO does not know these meter numbers.".to_string(),
        }
    }
}

/// A successful answer plus the backend that produced it.
#[derive(Debug, Clone)]
pub struct Reply {
    pub backend: Backend,
    pub data: Value,
}

/// Why a backend declined to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub code: i64,
    pub desc: String,
}

/// Returns the `data` payload when the envelope says success and carries data.
pub fn extract_data(response: &Value) -> Result<&Value, Rejection> {
    let code = response.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let desc = response
        .get("desc")
        .and_then(Value::as_str)
        .unwrap_or("no description")
        .to_string();
    match response.get("data") {
        Some(data) if code == 200 && !data.is_null() => Ok(data),
        _ => Err(Rejection { code, desc }),
    }
}

fn text_field(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `true` only for an explicit JSON `true` (the portal marks bad readings this way).
fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

fn number_field(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub account_no: String,
    pub meter_no: String,
    pub balance: f64,
    pub month_consumption: Option<f64>,
    /// Opaque: either `2026-10-18` or `2026-10-18 00:00:00`.
    pub reading_time: String,
    pub backend: Backend,
}

pub fn parse_balance(data: &Value, backend: Backend) -> Result<Balance, ApiError> {
    let balance = number_field(data, "balance")
        .ok_or_else(|| ApiError::Malformed("getBalance has no numeric 'balance'".to_string()))?;
    Ok(Balance {
        account_no: text_field(data, "accountNo").unwrap_or_default(),
        meter_no: text_field(data, "meterNo").unwrap_or_default(),
        balance,
        // Old API: currentMonthConsumption. Signed-in portal: currentMonthConsumedTaka.
        month_consumption: number_field(data, "currentMonthConsumption")
            .or_else(|| number_field(data, "currentMonthConsumedTaka")),
        reading_time: text_field(data, "readingTime").unwrap_or_default(),
        backend,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CustomerInfo {
    pub account_no: String,
    pub meter_no: String,
    pub customer_name: Option<String>,
    pub contact_no: Option<String>,
    pub sanction_load_kw: Option<f64>,
    pub phase_type: Option<String>,
    pub tariff: Option<String>,
    pub feeder: Option<String>,
    pub backend: Backend,
}

pub fn parse_customer_info(data: &Value, backend: Backend) -> CustomerInfo {
    CustomerInfo {
        account_no: text_field(data, "accountNo").unwrap_or_default(),
        meter_no: text_field(data, "meterNo").unwrap_or_default(),
        customer_name: text_field(data, "customerName"),
        contact_no: text_field(data, "contactNo"),
        sanction_load_kw: number_field(data, "sanctionLoad"),
        phase_type: text_field(data, "phaseType"),
        tariff: text_field(data, "tariffSolution"),
        feeder: text_field(data, "feederName"),
        backend,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recharge {
    pub date: String,
    pub total: f64,
    pub energy: Option<f64>,
    pub operator: Option<String>,
    pub status: Option<String>,
}

/// Newest first. The token number is deliberately not kept: it is a spendable secret.
pub fn parse_recharges(data: &Value) -> Result<Vec<Recharge>, ApiError> {
    let items = data
        .as_array()
        .ok_or_else(|| ApiError::Malformed("recharge history is not a list".to_string()))?;
    let mut out: Vec<Recharge> = items
        .iter()
        .map(|item| Recharge {
            // Real values look like "2026-10-06 16:50:23.0".
            date: text_field(item, "rechargeDate")
                .map(|d| d.trim_end_matches(".0").to_string())
                .unwrap_or_default(),
            total: number_field(item, "totalAmount").unwrap_or(0.0),
            energy: number_field(item, "energyAmount"),
            operator: text_field(item, "rechargeOperator"),
            status: text_field(item, "orderStatus"),
        })
        .collect();
    out.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonthUsage {
    pub month: String,
    /// `None` when the portal has no usable reading (missing, or flagged as excluded).
    pub units_kwh: Option<f64>,
    pub taka: f64,
    pub max_demand: Option<f64>,
}

/// Newest first.
pub fn parse_monthly(data: &Value) -> Result<Vec<MonthUsage>, ApiError> {
    let items = data
        .as_array()
        .ok_or_else(|| ApiError::Malformed("monthly consumption is not a list".to_string()))?;
    let mut out: Vec<MonthUsage> = items
        .iter()
        .map(|item| MonthUsage {
            month: text_field(item, "month").unwrap_or_default(),
            units_kwh: number_field(item, "consumedUnit")
                .filter(|_| !flag(item, "consumedUnitExcluded")),
            taka: number_field(item, "consumedTaka").unwrap_or(0.0),
            max_demand: number_field(item, "maximumDemand")
                .filter(|_| !flag(item, "maximumDemandExcluded")),
        })
        .collect();
    out.sort_by(|a, b| b.month.cmp(&a.month));
    Ok(out)
}

fn portal_backend(data: &Value) -> Backend {
    text_field(data, "meterType")
        .and_then(|t| Backend::parse(&t).ok())
        .unwrap_or(Backend::Unified)
}

/// Result of asking the meter for its balance right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Realtime {
    /// Read from the meter just now. `received_at` is the portal's RFC 3339 time (UTC).
    Fresh { balance: f64, received_at: String },
    /// No fresh reading. `reason` is a short lowercase phrase for people; `last` is the most
    /// recent realtime reading the portal still has (balance, RFC 3339 time).
    /// `retry_at` is when the portal says a new reading may be requested (RFC 3339), if known.
    Unavailable {
        reason: String,
        last: Option<(f64, String)>,
        retry_at: Option<String>,
    },
}

impl Realtime {
    fn unavailable(reason: &str, last: Option<(f64, String)>) -> Self {
        Self::Unavailable {
            reason: reason.trim().trim_end_matches('.').to_string(),
            last,
            retry_at: None,
        }
    }

    /// Builds the "no fresh reading" answer from a portal status entry. The wording is ours,
    /// so it is consistent and short whatever text the portal sends.
    fn from_status(entry: &Value, status: &str, last: Option<(f64, String)>) -> Self {
        let reason = match status {
            "DAILY_LIMIT_REACHED" => "today's realtime limit for this meter is used up".to_string(),
            "COOLDOWN_ACTIVE" => "a realtime reading was requested a moment ago".to_string(),
            "IN_PROGRESS" => "a realtime reading is already in progress".to_string(),
            "OFFLINE" => "the meter is offline".to_string(),
            "TIMEOUT" | "UPSTREAM_FAILURE" => "the meter did not respond in time".to_string(),
            _ => text_field(entry, "message").map_or_else(
                || format!("the portal answered {status}"),
                |m| m.trim_end_matches('.').to_string(),
            ),
        };
        Self::Unavailable {
            reason,
            last,
            retry_at: text_field(entry, "nextEligibleAt").or_else(|| text_field(entry, "retryAt")),
        }
    }
}

/// A random-looking UUID (version 4 layout) that is unique per call. The portal uses it to
/// recognise a repeated request, so retrying a failed POST cannot use up a second attempt.
fn idempotency_key() -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let digest = Sha256::digest(seed.as_bytes());
    let mut b = [0_u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = b.iter().fold(String::new(), |mut acc, x| {
        acc.push_str(&format!("{x:02x}"));
        acc
    });
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Portal sign-in credentials. The password never appears in `Debug` output.
#[derive(Clone)]
pub struct Credentials {
    username: String,
    password: String,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<hidden>")
            .finish()
    }
}

/// One account linked to the portal login, as listed by `GET /accounts`.
#[derive(Debug, Clone)]
struct PortalAccount {
    id: i64,
    account_no: String,
    meter_no: String,
}

/// Reads the `GET /accounts` answer: `{"accounts":[{accountId, accountNo, meterNo, ...}]}`.
/// Entries that are not active links are skipped.
fn parse_portal_accounts(body: &Value) -> Vec<PortalAccount> {
    body.get("accounts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|a| {
                    text_field(a, "linkStatus").is_none_or(|s| s.eq_ignore_ascii_case("ACTIVE"))
                })
                .filter_map(|a| {
                    Some(PortalAccount {
                        id: a.get("accountId")?.as_i64()?,
                        account_no: text_field(a, "accountNo")?,
                        meter_no: text_field(a, "meterNo").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct PortalState {
    logged_in: bool,
    accounts: Vec<PortalAccount>,
}

struct Portal {
    credentials: Credentials,
    base: Url,
    jar: Arc<Jar>,
    state: Mutex<PortalState>,
    /// Last read of the realtime summary (all meters in one call), reused for a short time.
    summary_cache: Mutex<Option<(std::time::Instant, Value)>>,
}

pub struct DescoClient {
    strict: Client,
    insecure: Client,
    base_url: String,
    retries: u32,
    use_insecure: AtomicBool,
    cookie_jar: Arc<Jar>,
    portal: Option<Portal>,
}

enum Attempt {
    Retry(ApiError),
    Fatal(ApiError),
    CertProblem(ApiError),
}

fn is_cert_error(err: &reqwest::Error) -> bool {
    let mut source = err.source();
    while let Some(inner) = source {
        let text = inner.to_string().to_lowercase();
        if text.contains("certificate")
            || text.contains("unknownissuer")
            || text.contains("invalid peer")
        {
            return true;
        }
        source = inner.source();
    }
    false
}

impl DescoClient {
    /// `insecure_tls` starts with certificate checks disabled. Otherwise strict TLS is tried
    /// first and relaxed automatically when DESCO's incomplete certificate chain is hit.
    pub fn new(
        base_url: &str,
        timeout: Duration,
        retries: u32,
        insecure_tls: bool,
    ) -> Result<Self, ApiError> {
        let cookie_jar = Arc::new(Jar::default());
        let build = |accept_invalid: bool| {
            Client::builder()
                .cookie_provider(Arc::clone(&cookie_jar))
                .timeout(timeout)
                .user_agent(concat!("desco-sentinel/", env!("CARGO_PKG_VERSION")))
                .danger_accept_invalid_certs(accept_invalid)
                .build()
                .map_err(|e| ApiError::Network(e.to_string()))
        };
        Ok(Self {
            strict: build(false)?,
            insecure: build(true)?,
            base_url: base_url.trim_end_matches('/').to_string(),
            retries,
            use_insecure: AtomicBool::new(insecure_tls),
            cookie_jar,
            portal: None,
        })
    }

    /// Switches to the signed-in portal API. The login happens lazily on the first request.
    pub fn with_credentials(mut self, credentials: Credentials) -> Result<Self, ApiError> {
        let base = Url::parse(&format!("{}/", self.base_url))
            .map_err(|e| ApiError::Malformed(format!("bad DESCO_BASE_URL: {e}")))?;
        self.portal = Some(Portal {
            credentials,
            base,
            jar: Arc::clone(&self.cookie_jar),
            state: Mutex::new(PortalState::default()),
            summary_cache: Mutex::new(None),
        });
        Ok(self)
    }

    fn attempt(
        &self,
        method: &Method,
        url: &str,
        params: &[(&str, &str)],
        body: Option<&Value>,
        xsrf: Option<&str>,
    ) -> Result<Value, Attempt> {
        let client = if self.use_insecure.load(Ordering::Relaxed) {
            &self.insecure
        } else {
            &self.strict
        };
        let mut request: RequestBuilder = client
            .request(method.clone(), url)
            .header("Accept", "application/json, text/plain, */*")
            .query(params);
        if let Some(token) = xsrf {
            request = request.header("X-XSRF-TOKEN", token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().map_err(|e| {
            let api_err = ApiError::Network(e.to_string());
            if is_cert_error(&e) {
                Attempt::CertProblem(api_err)
            } else {
                Attempt::Retry(api_err)
            }
        })?;

        let status = response.status();
        if status.is_server_error() {
            return Err(Attempt::Retry(ApiError::Http(status.as_u16())));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // Since Nov 2026 the portal answers "AUTHENTICATION_REQUIRED". Retrying or trying
            // the other backend cannot help.
            return Err(Attempt::Fatal(ApiError::AuthRequired(status.as_u16())));
        }
        if !status.is_success() {
            return Err(Attempt::Fatal(ApiError::Http(status.as_u16())));
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        response
            .json::<Value>()
            .map_err(|e| Attempt::Fatal(ApiError::Malformed(e.to_string())))
    }

    /// GET with retry and exponential backoff on timeouts and 5xx answers.
    fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Value, ApiError> {
        self.send_json(&Method::GET, url, params, None, None)
    }

    fn send_json(
        &self,
        method: &Method,
        url: &str,
        params: &[(&str, &str)],
        body: Option<&Value>,
        xsrf: Option<&str>,
    ) -> Result<Value, ApiError> {
        let mut tries_left = self.retries;
        let mut delay = Duration::from_millis(500);
        loop {
            match self.attempt(method, url, params, body, xsrf) {
                Ok(value) => return Ok(value),
                Err(Attempt::Fatal(e)) => return Err(e),
                Err(Attempt::CertProblem(e)) => {
                    if self.use_insecure.swap(true, Ordering::Relaxed) {
                        return Err(e);
                    }
                    warn!("certificate chain rejected, retrying with relaxed TLS");
                }
                Err(Attempt::Retry(e)) => {
                    if tries_left == 0 {
                        return Err(e);
                    }
                    tries_left -= 1;
                    warn!(error = %e, ?delay, "request failed, retrying");
                    thread::sleep(delay);
                    delay *= 2;
                }
            }
        }
    }

    // ---- signed-in portal API -------------------------------------------------------

    fn portal_url(portal: &Portal, path: &str) -> String {
        format!("{}{}", portal.base, path.trim_start_matches('/'))
    }

    fn xsrf_token(portal: &Portal) -> Option<String> {
        let header = portal.jar.cookies(&portal.base)?;
        header.to_str().ok()?.split(';').find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == "XSRF-TOKEN").then(|| value.to_string())
        })
    }

    /// Signs in and loads the list of linked accounts. Caller holds the state lock.
    fn portal_login(&self, portal: &Portal, state: &mut PortalState) -> Result<(), ApiError> {
        state.logged_in = false;
        // The csrf call hands out the XSRF cookie that the login must echo in a header.
        self.get_json(&Self::portal_url(portal, "auth/csrf"), &[])?;
        let token = Self::xsrf_token(portal);
        let body = json!({
            "username": portal.credentials.username,
            "password": portal.credentials.password,
        });
        let reply = self
            .send_json(
                &Method::POST,
                &Self::portal_url(portal, "auth/login"),
                &[],
                Some(&body),
                token.as_deref(),
            )
            .map_err(|e| match e {
                ApiError::AuthRequired(_) => {
                    ApiError::LoginFailed("DESCO rejected the username or password".to_string())
                }
                ApiError::Http(423) => {
                    ApiError::LoginFailed("the DESCO login is locked".to_string())
                }
                ApiError::Http(429) => {
                    ApiError::LoginFailed("too many sign-in attempts, try again later".to_string())
                }
                other => other,
            })?;
        if reply.get("authenticated").and_then(Value::as_bool) != Some(true) {
            return Err(ApiError::LoginFailed(
                "the portal did not confirm the sign-in".to_string(),
            ));
        }
        if reply.get("mustChangePassword").and_then(Value::as_bool) == Some(true) {
            return Err(ApiError::LoginFailed(
                "DESCO asks for a password change; do it once on the portal".to_string(),
            ));
        }
        state.accounts = self.portal_load_accounts(portal)?;
        state.logged_in = true;
        debug!(
            accounts = state.accounts.len(),
            "signed in to the DESCO portal"
        );
        Ok(())
    }

    fn portal_load_accounts(&self, portal: &Portal) -> Result<Vec<PortalAccount>, ApiError> {
        let body = self.get_json(&Self::portal_url(portal, "accounts"), &[])?;
        Ok(parse_portal_accounts(&body))
    }

    /// Finds the internal id for an identifier. Logs in first when needed and refreshes the
    /// account list once if the number is not known yet.
    fn portal_account(&self, portal: &Portal, id: &Identifier) -> Result<PortalAccount, ApiError> {
        let mut state = portal.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.logged_in {
            self.portal_login(portal, &mut state)?;
        }
        let find = |accounts: &[PortalAccount]| {
            accounts
                .iter()
                .find(|a| match id {
                    Identifier::Account(n) => a.account_no == n.as_str(),
                    Identifier::Meter(n) => a.meter_no == n.as_str(),
                })
                .cloned()
        };
        if let Some(found) = find(&state.accounts) {
            return Ok(found);
        }
        state.accounts = self.portal_load_accounts(portal)?;
        find(&state.accounts).ok_or_else(|| ApiError::NotFound {
            code: 404,
            desc: format!(
                "{} is not linked to the DESCO portal login (link it on the portal's Accounts page)",
                id.query_pair().1
            ),
        })
    }

    /// One portal request. Non-GET calls echo the XSRF cookie in a header. If the session has
    /// expired the client signs in again once and repeats the request.
    fn portal_call(
        &self,
        portal: &Portal,
        method: &Method,
        path: &str,
        params: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let url = Self::portal_url(portal, path);
        let run = || {
            let xsrf = if *method == Method::GET {
                None
            } else {
                Self::xsrf_token(portal)
            };
            self.send_json(method, &url, params, body, xsrf.as_deref())
        };
        match run() {
            Err(ApiError::AuthRequired(_)) => {
                {
                    let mut state = portal.state.lock().unwrap_or_else(PoisonError::into_inner);
                    self.portal_login(portal, &mut state)?;
                }
                run()
            }
            other => other,
        }
    }

    /// GET `/accounts/{id}/{suffix}` for the account behind `id`.
    fn portal_get(
        &self,
        portal: &Portal,
        id: &Identifier,
        suffix: &str,
        params: &[(&str, &str)],
    ) -> Result<Value, ApiError> {
        let account = self.portal_account(portal, id)?;
        self.portal_call(
            portal,
            &Method::GET,
            &format!("accounts/{}/{suffix}", account.id),
            params,
            None,
        )
    }

    /// The realtime summary for all meters, reused for 60 seconds so a status table of 15
    /// meters costs one call, not fifteen. `None` when it cannot be read.
    fn cached_realtime_summary(&self, portal: &Portal) -> Option<Value> {
        const TTL: Duration = Duration::from_secs(60);
        let mut cache = portal
            .summary_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((at, value)) = cache.as_ref()
            && at.elapsed() < TTL
        {
            return Some(value.clone());
        }
        let value = self
            .portal_call(portal, &Method::GET, "accounts/realtime-balance", &[], None)
            .ok()?;
        *cache = Some((std::time::Instant::now(), value.clone()));
        Some(value)
    }

    /// The daily snapshot is taken at midnight, so a recharge made later (or a realtime reading
    /// taken later) is missing from it. The portal itself shows the newest of the two. When the
    /// last realtime reading is from the snapshot's day or later, use it, so a recharged meter
    /// is not reported as low until the next snapshot. Best effort: any problem keeps the
    /// snapshot.
    fn prefer_newer_realtime(&self, portal: &Portal, id: &Identifier, balance: &mut Balance) {
        let Ok(account) = self.portal_account(portal, id) else {
            return;
        };
        let Some(summary) = self.cached_realtime_summary(portal) else {
            return;
        };
        let Some(entry) = summary
            .get("meters")
            .and_then(Value::as_array)
            .and_then(|meters| {
                meters
                    .iter()
                    .find(|m| m.get("accountId").and_then(Value::as_i64) == Some(account.id))
            })
        else {
            return;
        };
        let (Some(value), Some(at)) = (
            number_field(entry, "lastRealtimeBalance"),
            text_field(entry, "lastRealtimeReceivedAt"),
        ) else {
            return;
        };
        let when = crate::clock::rfc3339_to_dhaka(&at);
        if when.get(..10) >= balance.reading_time.get(..10) {
            balance.balance = value;
            balance.reading_time = when;
        }
    }

    /// Reads the balance straight from the meter, like the portal's "Get realtime balance"
    /// button. The portal allows only a few requests per meter per day, so the read-only
    /// summary is checked first: when the meter is not `AVAILABLE` no request is made (no
    /// attempt is used up) and the last realtime reading is returned instead. `wait` is how
    /// long to wait for the meter to answer; `poll` is the pause between status checks.
    pub fn realtime_balance(
        &self,
        id: &Identifier,
        wait: Duration,
        poll: Duration,
    ) -> Result<Realtime, ApiError> {
        let Some(portal) = &self.portal else {
            return Ok(Realtime::unavailable(
                "realtime balance needs the DESCO portal login (DESCO_USERNAME / DESCO_PASSWORD)",
                None,
            ));
        };
        let account = self.portal_account(portal, id)?;
        let summary =
            self.portal_call(portal, &Method::GET, "accounts/realtime-balance", &[], None)?;
        if summary.get("enabled").and_then(Value::as_bool) == Some(false) {
            return Ok(Realtime::unavailable(
                "DESCO has switched realtime balance off",
                None,
            ));
        }
        let entry = summary
            .get("meters")
            .and_then(Value::as_array)
            .and_then(|meters| {
                meters
                    .iter()
                    .find(|m| m.get("accountId").and_then(Value::as_i64) == Some(account.id))
            });
        let Some(entry) = entry else {
            return Ok(Realtime::unavailable(
                "DESCO does not offer realtime balance for this meter",
                None,
            ));
        };
        let last = number_field(entry, "lastRealtimeBalance").map(|b| {
            (
                b,
                text_field(entry, "lastRealtimeReceivedAt").unwrap_or_default(),
            )
        });
        let status = text_field(entry, "status").unwrap_or_default();
        if status != "AVAILABLE" {
            return Ok(Realtime::from_status(entry, &status, last));
        }

        let base = format!("accounts/{}/realtime-balance", account.id);
        let body = json!({ "idempotencyKey": idempotency_key() });
        let mut current = self.portal_call(portal, &Method::POST, &base, &[], Some(&body))?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            let status = text_field(&current, "status").unwrap_or_default();
            match status.as_str() {
                "SUCCESS" => {
                    let balance = number_field(&current, "balance").ok_or_else(|| {
                        ApiError::Malformed("realtime answer has no 'balance'".to_string())
                    })?;
                    return Ok(Realtime::Fresh {
                        balance,
                        received_at: text_field(&current, "receivedAt").unwrap_or_default(),
                    });
                }
                "IN_PROGRESS" => {
                    let request_id = text_field(&current, "requestId")
                        .filter(|r| {
                            r.chars()
                                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                        })
                        .ok_or_else(|| {
                            ApiError::Malformed(
                                "realtime request has no usable requestId".to_string(),
                            )
                        })?;
                    if std::time::Instant::now() >= deadline {
                        return Ok(Realtime::unavailable(
                            &format!(
                                "the meter did not answer within {} seconds; try again in a minute",
                                wait.as_secs()
                            ),
                            last,
                        ));
                    }
                    thread::sleep(poll);
                    current = self.portal_call(
                        portal,
                        &Method::GET,
                        &format!("{base}/{request_id}"),
                        &[],
                        None,
                    )?;
                }
                other => return Ok(Realtime::from_status(&current, other, last)),
            }
        }
    }

    /// Calls one endpoint on the hinted backend (default `unified`) and falls back to the
    /// other one when the first declines.
    pub fn call(
        &self,
        endpoint: &str,
        id: &Identifier,
        hint: Option<Backend>,
        extra: &[(&str, &str)],
    ) -> Result<Reply, ApiError> {
        let first = hint.unwrap_or(Backend::Unified);
        let mut last_rejection = Rejection {
            code: -1,
            desc: "no backend answered".to_string(),
        };
        for backend in [first, first.other()] {
            let url = format!("{}/{}/customer/{}", self.base_url, backend, endpoint);
            let mut params = vec![id.query_pair()];
            params.extend_from_slice(extra);
            let body = self.get_json(&url, &params)?;
            match extract_data(&body) {
                Ok(data) => {
                    return Ok(Reply {
                        backend,
                        data: data.clone(),
                    });
                }
                Err(rejection) => {
                    debug!(%backend, endpoint, code = rejection.code, "backend declined");
                    last_rejection = rejection;
                }
            }
        }
        Err(ApiError::NotFound {
            code: last_rejection.code,
            desc: last_rejection.desc,
        })
    }

    pub fn customer_info(
        &self,
        id: &Identifier,
        hint: Option<Backend>,
    ) -> Result<CustomerInfo, ApiError> {
        if let Some(portal) = &self.portal {
            let data = self.portal_get(portal, id, "summary", &[])?;
            return Ok(parse_customer_info(&data, portal_backend(&data)));
        }
        let reply = self.call("getCustomerInfo", id, hint, &[])?;
        Ok(parse_customer_info(&reply.data, reply.backend))
    }

    /// Current balance. If a meter number is not accepted by `getBalance` on either backend,
    /// the account number is resolved through `getCustomerInfo` and the call is retried.
    pub fn balance(&self, id: &Identifier, hint: Option<Backend>) -> Result<Balance, ApiError> {
        if let Some(portal) = &self.portal {
            let data = self.portal_get(portal, id, "balance", &[])?;
            let mut balance = parse_balance(&data, portal_backend(&data))?;
            self.prefer_newer_realtime(portal, id, &mut balance);
            return Ok(balance);
        }
        match self.call("getBalance", id, hint, &[]) {
            Ok(reply) => parse_balance(&reply.data, reply.backend),
            Err(ApiError::NotFound { .. }) if matches!(id, Identifier::Meter(_)) => {
                let info = self.customer_info(id, hint)?;
                let account = AccountNo::parse(&info.account_no)
                    .map_err(|e| ApiError::Malformed(e.to_string()))?;
                let reply = self.call(
                    "getBalance",
                    &Identifier::Account(account),
                    Some(info.backend),
                    &[],
                )?;
                parse_balance(&reply.data, reply.backend)
            }
            Err(e) => Err(e),
        }
    }

    pub fn recharge_history(
        &self,
        id: &Identifier,
        hint: Option<Backend>,
        date_from: &str,
        date_to: &str,
    ) -> Result<Vec<Recharge>, ApiError> {
        if let Some(portal) = &self.portal {
            let data = self.portal_get(
                portal,
                id,
                "transactions",
                &[("dateFrom", date_from), ("dateTo", date_to)],
            )?;
            return parse_recharges(&data);
        }
        let reply = self.call(
            "getRechargeHistory",
            id,
            hint,
            &[("dateFrom", date_from), ("dateTo", date_to)],
        )?;
        parse_recharges(&reply.data)
    }

    pub fn monthly_consumption(
        &self,
        id: &Identifier,
        hint: Option<Backend>,
        month_from: &str,
        month_to: &str,
    ) -> Result<Vec<MonthUsage>, ApiError> {
        if let Some(portal) = &self.portal {
            let data = self.portal_get(
                portal,
                id,
                "usage/monthly",
                &[("monthFrom", month_from), ("monthTo", month_to)],
            )?;
            return parse_monthly(&data);
        }
        let reply = self.call(
            "getCustomerMonthlyConsumption",
            id,
            hint,
            &[("monthFrom", month_from), ("monthTo", month_to)],
        )?;
        parse_monthly(&reply.data)
    }
}
