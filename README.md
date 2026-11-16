# desco-sentinel

[![CI](https://github.com/SAIFULLAH-SHARAFAT/desco-sentinel/actions/workflows/ci.yml/badge.svg)](https://github.com/SAIFULLAH-SHARAFAT/desco-sentinel/actions/workflows/ci.yml)

Watches many DESCO prepaid meters at once, warns you on Telegram or WhatsApp before a meter runs out of credit, and answers balance questions from your phone.

It was built for a building manager in Dhaka who looks after 15 to 20+ smart prepaid meters (apartments, lift, water pump). Checking each meter on the DESCO portal by hand does not scale, and a meter that reaches zero trips its relay.

| | |
|---|---|
| **Low-balance alerts** | Per-meter thresholds (default 150 BDT, for example 500 BDT for the lift). |
| **No alert spam** | A low meter is reported once, again only after a further drop of 25 BDT, and the record clears when the meter is recharged. |
| **Two-way bot** | `status`, `low`, `info`, the last 3 digits of an account or meter number (`825`) or a unit name (`lift`) return a phone-sized table or a detail card. |
| **Realtime balance** | A single-meter lookup also reads the balance from the meter itself, within the portal's daily limit. |
| **Configuration only** | Meters, thresholds, labels and providers are JSON and environment variables. No code changes. |
| **Small and self-contained** | One static Rust binary, synchronous HTTP, no database. |

> Not an official DESCO product and not affiliated with DESCO.

## Contents

[How it works](#how-it-works) ·
[Quick start](#quick-start) ·
[Configuration](#configuration) ·
[Commands](#commands) ·
[Telegram bot](#telegram-bot) ·
[Realtime balance](#realtime-balance) ·
[Deployment](#deployment) ·
[Operations](#operations) ·
[Security](#security) ·
[Reference](#reference) ·
[Development](#development)

## How it works

```
                          ┌──────────────────────────────┐
 you ── Telegram ───────► │ Cloudflare Worker (worker/)  │  checks Telegram's secret header
                          │ answers /help, relays the    │  and the allow-listed chat id
                          │ rest                         │
                          └──────────────┬───────────────┘
                                         │ workflow_dispatch (GitHub API)
                                         ▼
                          ┌──────────────────────────────┐        ┌──────────────────────┐
                          │ GitHub Actions: telegram-    │ HTTPS  │ prepaid.desco.org.bd │
                          │ reply  →  desco-sentinel     │ ◄────► │ signed-in portal API │
                          └──────────────┬───────────────┘        └──────────────────────┘
                                         │ Bot API
                                         ▼
                                    your Telegram chat

 GitHub Actions (cron): monitor every 6 h → alerts, weekly-report on Sundays
```

The DESCO portal requires a customer sign-in since 6 November 2026. The client logs in the way the website does (cookie session plus an XSRF header), lists the accounts linked to that login, and reads balance, customer summary, recharge history and monthly usage from the signed-in API. The 12-hour session is renewed automatically. Only meters **linked to the portal login** can be read.

## Quick start

Prerequisites: Rust 1.88 or newer, and a DESCO prepaid portal account with your meters linked (portal → Accounts → *Link a new account*, up to 20 per login).

```sh
cp config/meters.example.json config/meters.json   # your account / meter numbers
cp config/settings.example.env .env                # set DESCO_USERNAME and DESCO_PASSWORD

cargo run --release -- status                      # table of every meter
cargo run --release -- query 825                   # what the bot would answer
cargo run --release -- check --dry-run             # alerts that would be sent, nothing is sent
```

`.env`, `config/meters.json` and `state/` are git-ignored.

## Configuration

Everything is optional except the portal login and the meter list. The full list with comments is in [`config/settings.example.env`](config/settings.example.env).

### Portal login

| Variable | Meaning |
|---|---|
| `DESCO_USERNAME` | Phone number or email you registered on the portal. |
| `DESCO_PASSWORD` | Its password. Held in memory only; hidden from logs, errors and `Debug` output. |

Without them the tool falls back to DESCO's old public endpoints, which now answer `401`, and says so in plain words.

### Meter list

```json
[
  { "unit_name": "Mother Meter", "short_name": "Mother", "account_no": "98765401", "meter_no": "667788990001",
    "threshold_bdt": 150.0, "notify_phone": "8801700000000", "enabled": true },
  { "unit_name": "Lift and Pump", "short_name": "Lift", "account_no": "98765499", "threshold_bdt": 500.0 }
]
```

| Field | Meaning |
|---|---|
| `unit_name` | Unique display name. Also the key in the state file, so renaming a unit forgets its alert history. |
| `short_name` | Optional label for the narrow tables. |
| `account_no` / `meter_no` | Give either or both. A 12-digit number is a meter number, anything shorter an account number. The `DESCO-` prefix is accepted. The missing number is filled in from the portal, so last-3-digit lookups match both. |
| `identifier` | Alternative single field holding either kind of number. |
| `threshold_bdt` | Alert below this balance. Falls back to `DEFAULT_THRESHOLD_BDT`. |
| `notify_phone` | A tenant to message directly when `NOTIFY_TENANTS=true`. |
| `enabled` | `false` keeps the entry but skips it. |
| `backend_hint` | `unified` or `tkdes`; only used by the legacy public endpoints. |

On GitHub Actions put the whole array in the secret `DESCO_METERS_JSON`; it wins over the file. `--config <file>` selects another file.

### Labels

Messages name a meter by its account number with the unit name in brackets, for example `41167724 (Mother Meter)`. `LABEL_STYLE=name` gives the unit name only, with a separate `Acc#` column. In the narrow tables the bracketed name is shortened so every line stays within 34 columns; cards and alerts show it in full.

### Tuning

| Variable | Default | Meaning |
|---|---|---|
| `DEFAULT_THRESHOLD_BDT` | 150 | Alert level when a meter has none. |
| `FOLLOWUP_DROP_BDT` | 25 | Extra drop since the last alert that triggers a follow-up. |
| `RATE_LIMIT_MS` / `ON_DEMAND_DELAY_MS` | 1000 / 200 | Pause between meters in scheduled checks / in bot replies. |
| `MAX_RETRIES`, `REQUEST_TIMEOUT_SECS` | 2, 20 | Retry and timeout policy for portal requests. |
| `BALANCE_CACHE_SECS` | 300 | How long a bot reply may reuse a reading. |
| `REALTIME_ON_LOOKUP` | true | Ask the meter itself on single-meter lookups. |
| `REALTIME_MAX_METERS` | 2 | Lookups matching more meters skip realtime. |
| `REALTIME_WAIT_SECS` / `REALTIME_POLL_MS` | 45 / 2000 | How long to wait for the meter and how often to check. |
| `DESCO_BASE_URL`, `DESCO_INSECURE_TLS` | portal API, false | Override the endpoint or skip certificate checks. |

## Commands

| Command | What it does |
|---|---|
| `check` (default) | Poll enabled meters, apply the de-duplication rules, send alerts. Exits non-zero if a delivery failed or every meter was unreadable. |
| `check --dry-run` | Print the alerts, send nothing, write no state. |
| `status` | ASCII table of all meters plus the chat preview. |
| `info` | Directory: account, meter, customer, contact, load, tariff. |
| `query <text>` | Run the bot engine locally, for example `query 825` or `query low`. |
| `history <unit \| last-3 \| number>` | Last 5 recharges and recent monthly consumption. |
| `report` | Send the list of meters below their alert level, even when empty. |
| `reply --chat <id> --text <msg>` | Answer one chat message and exit (used by the relay). |
| `bot` | Answer Telegram by long polling. |
| `serve` | Webhook server for WhatsApp, with a background check every `CHECK_INTERVAL_HOURS`. |

## Telegram bot

| Send | You get |
|---|---|
| `/status`, `status` | Balance of every meter |
| `/low`, `low` | Only meters below their alert level, lowest first |
| `825` or `status 825` | Card for the meter whose account **or** meter number ends in 825, with a realtime reading |
| `lift` | Card by unit name (part of the name is enough) |
| `/info`, `info` | Directory of all meters |
| `/help` | Command list |

Only chats in `TELEGRAM_CHAT_ID` (or `TELEGRAM_ALLOWED_CHATS`) are answered. Anyone else is told their chat id so you can add them. Alert recipients and people allowed to ask questions are the same list.

## Realtime balance

The daily balance is a snapshot taken at midnight. Like the portal's **Get realtime balance** button, a lookup of one meter also asks the meter itself and shows the answer on the card:

```
🟢 Realtime balance: 971.11 BDT
⚡ Read from the meter at 2026-11-16 06:09
📅 Daily snapshot: 94.83 BDT
```

- The portal allows only a few realtime requests per meter per day. The summary is read first; when the limit is used up **no request is made**, so no attempt is spent. The card shows the snapshot and the last realtime reading with a short, polite explanation.
- A request carries an idempotency key, so a retried network call cannot consume a second attempt.
- An offline meter or one that does not answer within `REALTIME_WAIT_SECS` produces a note, not an error.
- `status`, `low` and scheduled alerts never request realtime balances.

## Deployment

### Scheduled alerts and the Telegram bot on GitHub Actions and Cloudflare (no server)

| Workflow | Trigger | Purpose |
|---|---|---|
| `ci.yml` | push, pull request | format, clippy, tests, dependency audit, worker tests |
| `release-binary.yml` | push to `main` touching code | builds the Linux binary and publishes it as the rolling `latest` release |
| `telegram-reply.yml` | started by the Worker | downloads `latest`, answers one message, exits |
| `monitor-cron.yml` | every 6 h, manual | `check`; de-duplication state kept in the Actions cache |
| `weekly-report.yml` | Sundays 11:30 Dhaka | `report` |

**1. GitHub repository secrets** (Settings → Secrets and variables → Actions) and variable:

| Name | Kind | Value |
|---|---|---|
| `DESCO_USERNAME`, `DESCO_PASSWORD` | secret | portal login |
| `DESCO_METERS_JSON` | secret | the meter list as one JSON line |
| `TELEGRAM_BOT_TOKEN` | secret | from @BotFather |
| `TELEGRAM_CHAT_ID` | secret | allowed chat ids, comma-separated |
| `NOTIFY_PROVIDER` | variable | `telegram` |

**2. Cloudflare Worker** named `desco-sentinel`, deployed from this repository (`wrangler.jsonc` points at `worker/src/index.js` and sets `GITHUB_REPO` and `GITHUB_REF`). Add four **runtime** secrets (Settings → Variables and secrets, not the build ones):

| Secret | Value |
|---|---|
| `TELEGRAM_BOT_TOKEN` | the bot token |
| `TELEGRAM_WEBHOOK_SECRET` | a long random string, also given to Telegram below |
| `ALLOWED_CHATS` | the same chat ids as `TELEGRAM_CHAT_ID` |
| `GITHUB_TOKEN` | fine-grained token for **this repository only**, permission *Actions: read and write* |

A missing or malformed secret makes the Worker answer authenticated calls with a 500 that names it.

**3. Register the webhook once**, with the same secret:

```sh
curl "https://api.telegram.org/bot<TOKEN>/setWebhook" \
  -d url=https://desco-sentinel.<account>.workers.dev \
  -d secret_token=<TELEGRAM_WEBHOOK_SECRET> \
  -d 'allowed_updates=["message"]'
```

Replies take about 30 seconds and cost about one Actions minute each. Message text reaches the program through environment variables, never through shell command text.

### Your own machine

`desco-sentinel bot` answers Telegram by long polling: no webhook, no public URL. While a webhook is registered Telegram refuses polling, so call `deleteWebhook` first. `scripts/windows-autostart.ps1` installs it as a Windows logon task.

### WhatsApp (`serve`)

Needs a process that stays up and a public HTTPS URL (a small VPS or container host):

```sh
docker build -t desco-sentinel .
docker run -d --env-file .env -v "$PWD/config:/app/config:ro" -v sentinel-state:/app/state -p 8080:8080 desco-sentinel
```

Point the provider's webhook at `https://<your-host>/webhook`. Meta: verify token `WA_VERIFY_TOKEN` and `WA_APP_SECRET` so `X-Hub-Signature-256` is checked. WAHA / Green-API: add `?token=<WEBHOOK_SECRET>` or an `X-Webhook-Token` header. `GET /health` returns `{"status":"ok"}`. **Set `ALLOWED_SENDERS`**: without it anyone who finds the bot can read balances, and customer names and phones are hidden from replies.

### Notification providers

Select with `NOTIFY_PROVIDER`: `telegram`, `whatsapp_meta` (WhatsApp Cloud API; scheduled alerts need an approved template with four body variables), `whatsapp_webhook` (self-hosted WAHA or Green-API) or `console`. Several recipients may be comma-separated; an alert counts as delivered when at least one received it.

## Operations

**When DESCO cannot be read.** If a check reads no meter at all, the tool says so instead of showing a misleading table: `status` and `low` explain the reason, and the scheduled check sends one message when readings stop and one when they return. Typical causes: wrong or changed password ("Could not sign in to the DESCO portal"), a meter not linked to the login ("not linked to the DESCO portal login"), or DESCO being down.

**Changing the DESCO password.** Update `DESCO_PASSWORD` in the GitHub secrets and in your local `.env`.

**Adding a meter.** Link it on the portal, add it to `DESCO_METERS_JSON` (and `config/meters.json`).

**Adding a person to the bot.** They press Start and send you the chat id the bot shows. Add it to `TELEGRAM_CHAT_ID` (GitHub) and `ALLOWED_CHATS` (Cloudflare).

**Expired GitHub token.** Users see "Could not start the check". Create a new fine-grained token and update the Worker secret `GITHUB_TOKEN`.

**Moving or renaming the repository.** GitHub secrets and variables do not move with it, and a fine-grained token stays bound to the old repository. Re-create the secrets, issue a new `GITHUB_TOKEN` for the new repository, update the Worker secret, reconnect Workers Builds to the new repository if you deploy through it, and let `release-binary` publish `latest` once.

### How alerts work

For every enabled meter on each check:

1. Balance at or above the threshold: forget any earlier alert.
2. Below the threshold and not alerted yet: send an alert and remember balance and reading time.
3. Below the threshold and already alerted: send a follow-up only if the reading time changed **and** the balance is at least 25 BDT lower than at the last alert.

A failed delivery is not recorded, so the next run retries it. Meters push readings to DESCO a few times a day, so polling every 4 to 6 hours is enough. `readingTime` is sometimes only a date, so follow-ups are at most daily.

## Security

See [SECURITY.md](SECURITY.md) for reporting a vulnerability.

- **No secret is stored in the repository.** `.env`, `config/meters.json` and `state/` are git-ignored; credentials live in encrypted GitHub Actions secrets and Cloudflare Worker secrets, both hidden in logs.
- The DESCO password is kept in memory only, never written to disk, logs or error text, and is sent only to the portal over HTTPS.
- The relay answers only allow-listed chats and only requests that carry Telegram's secret header (compared in constant time).
- The tool avoids putting customer names and phone numbers into group alerts; they are shown only to allow-listed senders.
- **Prefer a private repository.** The code holds nothing secret, but a public repository also publishes workflow run pages, including the text passed to `telegram-reply`. The workflows pin `RUST_LOG=warn` so run logs never contain meter names or balances.
- If a token or password was ever pasted into a chat, ticket or commit, treat it as leaked: revoke or change it and update the secrets.

## Reference

### DESCO portal API (signed in)

`GET /api/auth/csrf` hands out the `XSRF-TOKEN` cookie. `POST /api/auth/login` takes `{username, password}` with that value in the `X-XSRF-TOKEN` header and sets the `DESCO_SESSION` cookie (12 hours). Then:

| Call | Used for |
|---|---|
| `GET /api/accounts` | internal account ids for the linked accounts |
| `GET /api/accounts/{id}/balance` | daily balance snapshot |
| `GET /api/accounts/{id}/summary` | customer, load, tariff, feeder |
| `GET /api/accounts/{id}/transactions` | recharge history |
| `GET /api/accounts/{id}/usage/monthly` | monthly consumption (invalid months are excluded) |
| `GET /api/accounts/realtime-balance` | per-meter realtime availability and last reading |
| `POST /api/accounts/{id}/realtime-balance` | start a realtime read (`idempotencyKey`) |
| `GET /api/accounts/{id}/realtime-balance/{requestId}` | poll until `SUCCESS` or a failure status |

An expired session is renewed once per request. A TLS chain that the platform rejects makes the client retry with relaxed checks, or set `DESCO_INSECURE_TLS=true`.

### Legacy public API

Used only when no credentials are set. Two back ends, `unified` and `tkdes`; "not found" arrives as HTTP 200 with `data: null` or codes such as `16001`, so the rule is "code is not 200 or data is null", then try the other back end. Send exactly one of `accountNo` / `meterNo`.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests run offline: recorded payloads in `tests/fixtures`, a fake DESCO server for the sign-in, session-expiry and realtime flows, a fake notification provider, property tests for parsers and the alert state machine, and snapshot tests for message formats.

Layout: `domain.rs` (validated newtypes), `config.rs`, `desco_api.rs` (portal client), `state.rs`, `monitor.rs` (polling, caches, alert run), `formatter.rs` (pure text rendering), `query_engine.rs` (bot logic), `notifier.rs`, `telegram_bot.rs`, `webhook_server.rs`, `clock.rs`, `main.rs`, and `worker/` (the Cloudflare relay).

See [CHANGELOG.md](CHANGELOG.md) for what changed between versions.

## License

Source-available, free for non-commercial use. Commercial use requires the author's written permission and may require payment. See [LICENSE](LICENSE).
