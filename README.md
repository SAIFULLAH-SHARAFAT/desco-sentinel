# desco-sentinel

Monitor many DESCO prepaid meters at once, get a WhatsApp (or Telegram) alert before a meter runs out of credit, and ask the bot for balances from your phone.

Built for a building manager in Dhaka who looks after 20+ smart prepaid meters (apartments, lift, water pump). Checking each one by hand on the DESCO portal does not scale, and a meter that hits zero trips its relay.

- **Low-balance alerts** when a balance falls below a threshold (default 150 BDT, per-meter overrides such as 500 BDT for the lift).
- **No alert spam.** Each low meter is reported once, again only if a new reading shows it fell a further 25 BDT, and the record is cleared when the meter is recharged.
- **Two-way WhatsApp bot.** Send `status`, `info`, the last 3 digits of an account or meter number (`421`), or a unit name (`1A`) and get a phone-sized table or a detail card back.
- **Config only.** Add meters, change thresholds or switch notification provider by editing JSON / environment variables. No code changes.
- Single static binary, synchronous HTTP, no database.

## Quick start

```sh
cp config/meters.example.json config/meters.json   # edit: your real account and/or meter numbers
cp config/settings.example.env .env                # optional settings and secrets

cargo run --release -- status                      # table of every meter
cargo run --release -- query 421                   # what the WhatsApp bot would answer
cargo run --release -- check --dry-run             # print the alerts that would be sent
```

`config/meters.json`, `.env` and `state/` are git-ignored.

### Meter list

```json
[
  { "unit_name": "Apt 1A", "short_name": "Apt 1A", "account_no": "98765401", "meter_no": "667788990001",
    "threshold_bdt": 150.0, "notify_phone": "8801700000000", "enabled": true },
  { "unit_name": "Common - Lift & Pump", "short_name": "Lift", "account_no": "98765499",
    "threshold_bdt": 500.0, "backend_hint": "unified" }
]
```

| Field | Meaning |
|---|---|
| `unit_name` | Unique display name. Also the key in the state file, so renaming a unit forgets its alert history. |
| `short_name` | Optional label for narrow WhatsApp tables (about 9 characters fit). |
| `account_no` / `meter_no` | Give either one or both. A 12-digit number is a meter number, anything shorter is an account number. The `DESCO-` prefix is accepted. The missing number is filled in from the API, so last-3-digit lookups match both. |
| `identifier` | Alternative single field holding either kind of number. |
| `threshold_bdt` | Alert below this balance. Falls back to `DEFAULT_THRESHOLD_BDT`. |
| `notify_phone` | Tenant to message directly when `NOTIFY_TENANTS=true`. |
| `enabled` | `false` keeps the entry but skips it. |
| `backend_hint` | `unified` or `tkdes`. Skips a wasted first call when you know where the meter lives. The program also learns this by itself. |

For GitHub Actions, put the whole array in a secret named `DESCO_METERS_JSON`. If that variable is set it wins over the file. `--config <file>` selects another file.

## Commands

| Command | What it does |
|---|---|
| `check` (default) | Poll enabled meters, apply the de-duplication rules, send alerts. Exit code is non-zero if a delivery failed or every meter was unreadable. |
| `check --dry-run` | Print the alerts, send nothing, write no alert state. |
| `status` | ASCII table of all meters plus the WhatsApp preview. |
| `info` | Directory: account, meter, customer name, contact, load, tariff. |
| `query <text>` | Run the bot engine locally, for example `query 421` or `query status`. |
| `serve` | Webhook server for the two-way bot, with a background check every `CHECK_INTERVAL_HOURS`. |
| `history <unit \| last-3 \| number>` | Last 5 recharges and recent monthly consumption. Works for numbers that are not in your config too. |

### What the bot understands

| Message | Reply |
|---|---|
| `all`, `status`, `table`, `list`, `balance` | Table of every meter, each line at most 34 columns so it does not wrap on a phone |
| `info`, `meters`, `directory` | Compact directory (customer names only for allow-listed senders) |
| `421` (3 digits) | Card for every meter whose account **or** meter number ends in 421 |
| `1A`, `lift` | Card for the matching unit(s) |
| `help`, anything else | Command menu |

## Notifications

Select with `NOTIFY_PROVIDER`:

- `whatsapp_meta`: [WhatsApp Cloud API](https://developers.facebook.com/docs/whatsapp/cloud-api). Needs `WA_PHONE_NUMBER_ID`, `WA_ACCESS_TOKEN`, `RECIPIENT_PHONE`.
  Meta only delivers free-form text to numbers that messaged you in the last 24 hours. Scheduled alerts therefore need an approved **message template** with four body variables (`{{1}}` unit, `{{2}}` balance, `{{3}}` threshold, `{{4}}` details). Set `WA_TEMPLATE_NAME` (and `WA_TEMPLATE_LANG`). Replies to bot queries are plain text, which is allowed inside the 24-hour window.
  The Graph API version is configurable with `WA_GRAPH_VERSION`.
- `whatsapp_webhook`: self-hosted [WAHA](https://waha.devlike.pro/) or [Green-API](https://green-api.com/) using a QR-linked number. Set `WA_WEBHOOK_URL`, `WA_CHAT_ID` (a group or a number), and for WAHA `WA_WEBHOOK_TOKEN`. Use `WA_WEBHOOK_FORMAT=green` for Green-API.
- `telegram`: `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`.
- `console`: print to stdout (default).

All variables are listed in [`config/settings.example.env`](config/settings.example.env).

## Deployment

### Scheduled alerts on GitHub Actions

`.github/workflows/monitor-cron.yml` runs `check` every 6 hours and on demand (with a dry-run option). Add your meter list and provider credentials as repository secrets and variables (names are in the workflow). The de-duplication state is kept in the Actions cache between runs, so you are not re-alerted every 6 hours. If the cache is ever evicted you get at most one repeat alert per low meter.

Scheduled workflows in public repositories are paused after 60 days without repository activity. Keep the repository private (it holds nothing secret, but it is your building's setup) or re-enable the schedule when GitHub asks.

### Two-way bot (`serve`)

The bot needs a process that stays up and a public HTTPS URL, so it cannot run on Actions. Use a small VPS or container host:

```sh
docker build -t desco-sentinel .
docker run -d --env-file .env -v "$PWD/config:/app/config:ro" -v sentinel-state:/app/state -p 8080:8080 desco-sentinel
```

Point the provider's webhook at `https://<your-host>/webhook`:

- **Meta**: callback URL as above, verify token = `WA_VERIFY_TOKEN`, and set `WA_APP_SECRET` so the `X-Hub-Signature-256` header is checked.
- **WAHA / Green-API**: add `?token=<WEBHOOK_SECRET>` to the URL (or send an `X-Webhook-Token` header).

`GET /health` returns `{"status":"ok"}`.

**Set `ALLOWED_SENDERS`** (comma-separated phone numbers). Without it anyone who finds the bot can read balances, and customer names and phone numbers are hidden from replies. DESCO's API exposes names, addresses and phones for any account number, so this tool avoids putting them into group alerts and only shows them to allow-listed senders.

## How alerts work

For every enabled meter on each check:

1. Balance at or above the threshold: forget any earlier alert for that meter.
2. Below the threshold and not alerted yet: send an alert and remember balance and reading time.
3. Below the threshold and already alerted: send a follow-up only if the reading time changed **and** the balance is at least 25 BDT lower than at the last alert.

A failed delivery is not recorded, so the next run retries it. Meters push readings to DESCO a few times a day, which is why polling every 4 to 6 hours is enough. `readingTime` is sometimes only a date, so follow-ups are at most daily.

## DESCO API notes

The portal's public JSON API is used without login. Behaviour this project relies on, checked against the live service:

- Two back ends, `unified` and `tkdes`. A meter normally exists on one. "Not found" arrives as HTTP 200 with `data: null` or with codes such as `16001` / `16002`, so the fallback rule is "code is not 200 or data is null", then try the other back end.
- Send exactly one of `accountNo` / `meterNo`. A 12-digit value is a meter number; passing a mismatched pair returns no data. `DESCO-` prefixes are not understood by the API and are stripped.
- `readingTime` can be `2026-10-18` or `2026-10-18 00:00:00`; it is treated as an opaque string.
- The TLS chain has been incomplete in the past. Strict TLS is tried first and relaxed automatically if the certificate is rejected (or set `DESCO_INSECURE_TLS=true`).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests run offline: sample payloads in `tests/fixtures`, a local fake DESCO server for fallback / retry behaviour, a local fake provider for notification requests, property tests for parsers and the alert state machine, and snapshot tests for message formats.

Layout: `domain.rs` (validated newtypes), `config.rs`, `desco_api.rs`, `state.rs`, `monitor.rs` (polling, caches, alert run), `formatter.rs`, `query_engine.rs`, `notifier.rs`, `webhook_server.rs`, `main.rs`.

## License

Source-available, free for non-commercial use. Commercial use requires the author's written permission and may require payment. See [LICENSE](LICENSE). This is not an official DESCO product and is not affiliated with DESCO.
