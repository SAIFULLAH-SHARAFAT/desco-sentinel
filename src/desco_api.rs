//! DESCO prepaid API client.
//!
//! Behaviour verified against the live service:
//! * Two backends (`unified`, `tkdes`). A meter normally exists on only one.
//! * "Not found" comes back as HTTP 200 with `data: null` (unified getBalance) or with
//!   codes such as 16001 / 16002, so the fallback rule is "code != 200 or data is null".
//! * A 12 digit identifier is a `meterNo`; anything shorter is an `accountNo`.
//!   Sending both only works when they belong together, so we send exactly one.
//! * `readingTime` may be a plain date (`2026-10-18`), so it is kept as an opaque string.

use std::error::Error as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;
use thiserror::Error;
use tracing::{debug, warn};

use crate::domain::{AccountNo, Backend, Identifier};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {0} from DESCO")]
    Http(u16),
    #[error("unexpected response: {0}")]
    Malformed(String),
    #[error("meter not found on any backend: {desc} (code {code})")]
    NotFound { code: i64, desc: String },
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
        month_consumption: number_field(data, "currentMonthConsumption"),
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
    pub units_kwh: f64,
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
            units_kwh: number_field(item, "consumedUnit").unwrap_or(0.0),
            taka: number_field(item, "consumedTaka").unwrap_or(0.0),
            max_demand: number_field(item, "maximumDemand"),
        })
        .collect();
    out.sort_by(|a, b| b.month.cmp(&a.month));
    Ok(out)
}

pub struct DescoClient {
    strict: Client,
    insecure: Client,
    base_url: String,
    retries: u32,
    use_insecure: AtomicBool,
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
        let build = |accept_invalid: bool| {
            Client::builder()
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
        })
    }

    fn attempt(&self, url: &str, params: &[(&str, &str)]) -> Result<Value, Attempt> {
        let client = if self.use_insecure.load(Ordering::Relaxed) {
            &self.insecure
        } else {
            &self.strict
        };
        let response = client
            .get(url)
            .header("Accept", "application/json, text/plain, */*")
            .query(params)
            .send()
            .map_err(|e| {
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
        if !status.is_success() {
            return Err(Attempt::Fatal(ApiError::Http(status.as_u16())));
        }
        response
            .json::<Value>()
            .map_err(|e| Attempt::Fatal(ApiError::Malformed(e.to_string())))
    }

    /// GET with retry and exponential backoff on timeouts and 5xx answers.
    fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Value, ApiError> {
        let mut tries_left = self.retries;
        let mut delay = Duration::from_millis(500);
        loop {
            match self.attempt(url, params) {
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
        let reply = self.call("getCustomerInfo", id, hint, &[])?;
        Ok(parse_customer_info(&reply.data, reply.backend))
    }

    /// Current balance. If a meter number is not accepted by `getBalance` on either backend,
    /// the account number is resolved through `getCustomerInfo` and the call is retried.
    pub fn balance(&self, id: &Identifier, hint: Option<Backend>) -> Result<Balance, ApiError> {
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
        let reply = self.call(
            "getCustomerMonthlyConsumption",
            id,
            hint,
            &[("monthFrom", month_from), ("monthTo", month_to)],
        )?;
        parse_monthly(&reply.data)
    }
}
