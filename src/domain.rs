//! Validated domain values. Raw strings are parsed once here, so the rest of the
//! code never has to re-check whether an account number looks sane.

use std::fmt;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error(
        "'{0}' is not a valid DESCO identifier (expected an account number of 6-11 digits or a 12 digit meter number)"
    )]
    BadIdentifier(String),
    #[error("'{0}' is not a valid account number")]
    BadAccount(String),
    #[error("'{0}' is not a valid 12 digit meter number")]
    BadMeter(String),
    #[error("'{0}' is not exactly 3 digits")]
    BadLast3(String),
    #[error("unknown backend '{0}' (use 'unified' or 'tkdes')")]
    BadBackend(String),
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn last_n(s: &str, n: usize) -> &str {
    &s[s.len().saturating_sub(n)..]
}

/// Customer account number, e.g. `98765421`. The `DESCO-` prefix is accepted and dropped
/// because the API returns no data when the prefix is sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountNo(String);

impl AccountNo {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let trimmed = raw.trim();
        let digits = match trimmed.get(..6) {
            Some(prefix) if prefix.eq_ignore_ascii_case("DESCO-") => &trimmed[6..],
            _ => trimmed,
        };
        // Meter numbers are 12 digits; accounts are shorter.
        if all_digits(digits) && (6..=11).contains(&digits.len()) {
            Ok(Self(digits.to_string()))
        } else {
            Err(DomainError::BadAccount(raw.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn last3(&self) -> &str {
        last_n(&self.0, 3)
    }
}

impl fmt::Display for AccountNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 12 digit meter number, e.g. `667788990011`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeterNo(String);

impl MeterNo {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let trimmed = raw.trim();
        if all_digits(trimmed) && trimmed.len() == 12 {
            Ok(Self(trimmed.to_string()))
        } else {
            Err(DomainError::BadMeter(raw.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn last3(&self) -> &str {
        last_n(&self.0, 3)
    }
}

impl fmt::Display for MeterNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What we send to the API: either a meter number or an account number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identifier {
    Account(AccountNo),
    Meter(MeterNo),
}

impl Identifier {
    /// 12 digits means meter number, anything else is treated as an account number.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        if let Ok(meter) = MeterNo::parse(raw) {
            return Ok(Self::Meter(meter));
        }
        AccountNo::parse(raw)
            .map(Self::Account)
            .map_err(|_| DomainError::BadIdentifier(raw.to_string()))
    }

    /// The query-string pair for DESCO endpoints.
    #[must_use]
    pub fn query_pair(&self) -> (&'static str, &str) {
        match self {
            Self::Account(a) => ("accountNo", a.as_str()),
            Self::Meter(m) => ("meterNo", m.as_str()),
        }
    }
}

/// Last three digits typed by a user, e.g. `421`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Last3(String);

impl Last3 {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let trimmed = raw.trim();
        if all_digits(trimmed) && trimmed.len() == 3 {
            Ok(Self(trimmed.to_string()))
        } else {
            Err(DomainError::BadLast3(raw.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// DESCO runs two separate backends. Most accounts live on exactly one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    Unified,
    Tkdes,
}

impl Backend {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "unified" => Ok(Self::Unified),
            "tkdes" => Ok(Self::Tkdes),
            _ => Err(DomainError::BadBackend(raw.to_string())),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unified => "unified",
            Self::Tkdes => "tkdes",
        }
    }

    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::Unified => Self::Tkdes,
            Self::Tkdes => Self::Unified,
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Amount in BDT. Wrapped so a balance cannot be confused with a threshold by accident
/// at function boundaries.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Bdt(pub f64);

impl fmt::Display for Bdt {
    /// `1,384.35`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.0 < 0.0;
        let text = format!("{:.2}", self.0.abs());
        let (int_part, frac_part) = text.split_once('.').unwrap_or((&text, "00"));
        let mut grouped = String::new();
        for (i, ch) in int_part.chars().enumerate() {
            if i > 0 && (int_part.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        let sign = if negative { "-" } else { "" };
        write!(f, "{sign}{grouped}.{frac_part}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_detects_meter_vs_account() {
        assert!(matches!(
            Identifier::parse("667788990011"),
            Ok(Identifier::Meter(_))
        ));
        assert!(matches!(
            Identifier::parse("98765421"),
            Ok(Identifier::Account(_))
        ));
        assert!(matches!(
            Identifier::parse("DESCO-98765421"),
            Ok(Identifier::Account(a)) if a.as_str() == "98765421"
        ));
        assert!(Identifier::parse("abc").is_err());
        assert!(Identifier::parse("").is_err());
    }

    #[test]
    fn bdt_display_groups_thousands() {
        assert_eq!(Bdt(1384.35).to_string(), "1,384.35");
        assert_eq!(Bdt(95.5).to_string(), "95.50");
        assert_eq!(Bdt(1_234_567.0).to_string(), "1,234,567.00");
        assert_eq!(Bdt(-12.3).to_string(), "-12.30");
    }
}
