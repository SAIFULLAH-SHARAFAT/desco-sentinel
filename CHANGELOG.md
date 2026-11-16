# Changelog

All notable changes are listed here, newest first.

## Unreleased

### Added
- **Signed-in DESCO portal client.** DESCO requires a customer sign-in since 6 November 2026. With `DESCO_USERNAME` and `DESCO_PASSWORD` the client logs in (cookie session and XSRF header), resolves the portal's internal account ids and reads balance, summary, transactions and monthly usage. The session is renewed on expiry. A wrong password, a locked login and a throttled login each give a clear message.
- **Realtime balance on single-meter lookups.** Looking up one meter also reads the balance from the meter itself. The portal summary is checked first, so no daily attempt is spent when the limit is already used; the last realtime reading is shown instead. Controlled by `REALTIME_ON_LOOKUP`, `REALTIME_MAX_METERS`, `REALTIME_WAIT_SECS` and `REALTIME_POLL_MS`.
- **Account-first labels** such as `41167724 (Mother Meter)`, selectable with `LABEL_STYLE=account|name`; narrow tables stay within 34 columns.
- `SECURITY.md`, this changelog, and `.gitattributes` for consistent line endings.

### Fixed
- A meter recharged after the midnight snapshot was reported as low (and could trigger a false alert) until the next snapshot. The latest portal realtime reading is now preferred when it is from the snapshot's day or later.

### Changed
- Months that the portal flags as excluded or missing now show `-` instead of a zero.
- The month-usage figure accepts both the old and the new field name.
- README rewritten: architecture overview, configuration reference, deployment and operations runbook, security notes.

### Security
- Credentials are wrapped so they never appear in `Debug` output, logs or errors.

## 0.1.0

Initial version: multi-meter balance monitor with threshold alerts and de-duplication, two-way bot for WhatsApp and Telegram, Cloudflare relay with GitHub Actions replies, weekly report, outage and recovery notices.
