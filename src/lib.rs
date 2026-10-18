//! desco-sentinel: monitors DESCO prepaid meters, sends low-balance alerts
//! and answers on-demand WhatsApp queries.

pub mod clock;
pub mod config;
pub mod desco_api;
pub mod domain;
pub mod formatter;
pub mod monitor;
pub mod notifier;
pub mod query_engine;
pub mod state;
pub mod webhook_server;
