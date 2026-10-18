//! desco-sentinel command line entry point.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use desco_sentinel::clock;
use desco_sentinel::config::{Env, Settings, load_meters};
use desco_sentinel::desco_api::{Balance, DescoClient};
use desco_sentinel::domain::Identifier;
use desco_sentinel::formatter::{
    StatusRow, ascii_table, monthly_table, recharge_table, whatsapp_status,
};
use desco_sentinel::monitor::Monitor;
use desco_sentinel::notifier::Notifier;
use desco_sentinel::query_engine::handle_incoming_query;
use desco_sentinel::webhook_server;

#[derive(Parser)]
#[command(
    name = "desco-sentinel",
    version,
    about = "Monitor DESCO prepaid meters, send low-balance alerts and answer WhatsApp queries"
)]
struct Cli {
    /// Meter list (default: DESCO_METERS_JSON env var, else config/meters.json)
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Poll all enabled meters and send low-balance alerts (default)
    Check {
        /// Print the alerts instead of sending them; no state is written
        #[arg(long)]
        dry_run: bool,
    },
    /// Table of all meters (terminal table plus the WhatsApp preview)
    Status,
    /// Directory of all meters: account, meter, customer, contact, load, tariff
    Info,
    /// Run the WhatsApp bot engine locally, e.g. `query 421` or `query status`
    Query {
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Start the webhook server (two-way WhatsApp bot) with periodic checks.
    /// With NOTIFY_PROVIDER=telegram it also answers Telegram messages
    Serve,
    /// Answer Telegram messages (`status`, `825`, `help`) until stopped; no public URL needed
    Bot,
    /// Send the weekly low-balance report (meters below their alert level) to the recipients
    Report {
        /// Print the report instead of sending it
        #[arg(long)]
        dry_run: bool,
    },
    /// Last 5 recharges and recent monthly consumption for a unit, last-3 digits or number
    History { identifier: String },
}

fn init_logging(verbose: bool) {
    let default = if verbose { "info" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}

fn build_monitor(cli: &Cli, env: &Env) -> Result<Monitor> {
    let settings = Settings::from_env(env)?;
    let meters = load_meters(env, cli.config.as_deref(), settings.default_threshold)
        .context("could not load the meter list")?;
    let api = DescoClient::new(
        &settings.base_url,
        settings.request_timeout,
        settings.retries,
        settings.insecure_tls,
    )?;
    Ok(Monitor::new(api, meters, settings))
}

fn build_notifier(env: &Env, monitor: &Monitor) -> Result<Notifier> {
    Notifier::from_env(env, monitor.settings()).context("notification setup is incomplete")
}

fn run(cli: &Cli, env: &Env) -> Result<ExitCode> {
    let command = cli
        .command
        .as_ref()
        .unwrap_or(&Command::Check { dry_run: false });
    let monitor = build_monitor(cli, env)?;

    match command {
        Command::Check { dry_run } => {
            // Dry runs never send, so notification settings are not needed.
            let notifier = if *dry_run {
                Notifier::console()
            } else {
                build_notifier(env, &monitor)?
            };
            let report = monitor.run_check(&notifier, *dry_run);
            println!(
                "Checked {} meter(s): {} low, {} alert(s) {}, {} unreadable, {} delivery failure(s)",
                report.checked,
                report.low,
                if *dry_run {
                    report.messages.len()
                } else {
                    report.alerts_sent
                },
                if *dry_run { "would be sent" } else { "sent" },
                report.unreadable,
                report.notify_failures
            );
            let all_failed = report.checked > 0 && report.unreadable == report.checked;
            if report.notify_failures > 0 || all_failed {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Status => print_status(&monitor),
        Command::Info => print_info(&monitor),
        Command::Query { text } => {
            println!("{}", handle_incoming_query(&text.join(" "), &monitor, true));
        }
        Command::Serve => {
            let notifier = Arc::new(build_notifier(env, &monitor)?);
            webhook_server::serve(&Arc::new(monitor), &notifier, env)?;
        }
        Command::Report { dry_run } => {
            let notifier = if *dry_run {
                Notifier::console()
            } else {
                build_notifier(env, &monitor)?
            };
            let text = desco_sentinel::query_engine::weekly_report(&monitor);
            notifier
                .send_broadcast(&text)
                .context("could not send the weekly report")?;
        }
        Command::Bot => {
            let notifier = Notifier::telegram(env, monitor.settings())
                .context("Telegram setup is incomplete (TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID)")?;
            desco_sentinel::telegram_bot::run(&monitor, &notifier, env)?;
        }
        Command::History { identifier } => print_history(&monitor, identifier)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn print_status(monitor: &Monitor) {
    let readings = monitor.poll_all(
        std::time::Duration::ZERO,
        monitor.settings().on_demand_delay,
    );
    let mut table_rows = Vec::new();
    let mut preview_rows = Vec::new();
    for (meter, result) in &readings {
        let known = monitor.known(meter);
        let (account, meter_no) = (
            known.account.map(|a| a.to_string()).unwrap_or_default(),
            known.meter.map(|m| m.to_string()).unwrap_or_default(),
        );
        match result {
            Ok(b) => {
                let low = b.balance < meter.threshold.0;
                table_rows.push(vec![
                    meter.unit_name.clone(),
                    account.clone(),
                    meter_no,
                    b.backend.to_string(),
                    format!("{:.2}", b.balance),
                    b.month_consumption
                        .map_or_else(|| "-".to_string(), |u| format!("{u:.2}")),
                    b.reading_time.clone(),
                    if low { "LOW" } else { "OK" }.to_string(),
                ]);
                preview_rows.push(status_row(
                    meter.label(),
                    &account,
                    Some(b),
                    meter.threshold.0,
                ));
            }
            Err(e) => {
                table_rows.push(vec![
                    meter.unit_name.clone(),
                    account.clone(),
                    meter_no,
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    format!("ERROR: {e}"),
                ]);
                preview_rows.push(status_row(meter.label(), &account, None, meter.threshold.0));
            }
        }
    }
    println!(
        "{}",
        ascii_table(
            &[
                "Unit",
                "Account No",
                "Meter No",
                "Backend",
                "Balance (BDT)",
                "Month Usage (BDT)",
                "Reading Time",
                "Status",
            ],
            &table_rows,
        )
    );
    println!("\nWhatsApp preview:\n");
    println!(
        "{}",
        whatsapp_status(&preview_rows, &clock::format_minute(clock::dhaka_now()))
    );
}

fn status_row(label: &str, account: &str, balance: Option<&Balance>, threshold: f64) -> StatusRow {
    StatusRow {
        label: label.to_string(),
        number: account.to_string(),
        balance: balance.map(|b| b.balance),
        threshold,
    }
}

fn print_info(monitor: &Monitor) {
    let delay = monitor.settings().on_demand_delay;
    let meters: Vec<_> = monitor.enabled().collect();
    let mut rows = Vec::new();
    for (i, meter) in meters.iter().enumerate() {
        let dash = || "-".to_string();
        match monitor.info(meter) {
            Ok(info) => rows.push(vec![
                meter.unit_name.clone(),
                info.account_no,
                info.meter_no,
                info.customer_name.unwrap_or_else(dash),
                info.contact_no.unwrap_or_else(dash),
                info.sanction_load_kw.map_or_else(dash, |l| format!("{l}")),
                info.tariff.unwrap_or_else(dash),
            ]),
            Err(e) => rows.push(vec![
                meter.unit_name.clone(),
                dash(),
                dash(),
                format!("ERROR: {e}"),
                dash(),
                dash(),
                dash(),
            ]),
        }
        if i + 1 < meters.len() {
            std::thread::sleep(delay);
        }
    }
    println!(
        "{}",
        ascii_table(
            &[
                "Unit",
                "Account No",
                "Meter No",
                "Customer Name",
                "Contact No",
                "Load (kW)",
                "Tariff"
            ],
            &rows,
        )
    );
}

fn print_history(monitor: &Monitor, identifier: &str) -> Result<()> {
    // A configured meter (by unit name / last 3 / number) or any raw account or meter number.
    let (title, id, hint) = match monitor.find_configured(identifier) {
        Some(meter) => {
            let (id, hint) = (
                meter.identifier().context("meter has no identifier")?,
                monitor.known(meter).backend,
            );
            (meter.unit_name.clone(), id, hint)
        }
        None => match Identifier::parse(identifier) {
            Ok(id) => (identifier.to_string(), id, None),
            Err(e) => bail!("'{identifier}' matches no configured meter and {e}"),
        },
    };

    // History endpoints are keyed by account number, so resolve it through the balance call.
    let balance = monitor.api().balance(&id, hint)?;
    let account = Identifier::parse(&balance.account_no)?;
    let backend = Some(balance.backend);

    let now = clock::dhaka_now();
    let since = clock::days_ago(180);
    let recharges = monitor.api().recharge_history(
        &account,
        backend,
        &clock::format_day(since),
        &clock::format_day(now),
    )?;
    let months = monitor.api().monthly_consumption(
        &account,
        backend,
        &clock::format_month(since),
        &clock::format_month(now),
    )?;

    println!(
        "{title}  (account {}, meter {}, balance {:.2} BDT)\n",
        balance.account_no, balance.meter_no, balance.balance
    );
    println!("Last recharges:");
    let last_five: Vec<_> = recharges.into_iter().take(5).collect();
    println!("{}\n", recharge_table(&last_five));
    println!("Monthly consumption:");
    println!("{}", monthly_table(&months));
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let verbose = matches!(
        cli.command,
        None | Some(Command::Serve | Command::Bot | Command::Check { .. })
    );
    init_logging(verbose);
    let env = Env::load();
    match run(&cli, &env) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
