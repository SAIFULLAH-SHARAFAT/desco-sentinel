# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use GitHub's private reporting:
**Security → Report a vulnerability** on this repository. Include what you found, how to reproduce it, and the impact you expect. You will get an answer as soon as the maintainer can read it.

## Scope

In scope: leaking or logging of credentials, bypass of the chat allow-list or the Telegram secret-header check in `worker/`, injection through message text, and exposure of customer data (names, phone numbers) to non-allow-listed senders.

Out of scope: weaknesses in the DESCO portal itself (report those to DESCO) and findings that need access to the maintainer's own secrets.

## How secrets are handled

- Credentials are read from environment variables or the git-ignored `.env`, kept in memory, and never written to disk, logs or error messages. The DESCO password is wrapped in a type whose `Debug` output hides it.
- CI and runtime secrets live in GitHub Actions secrets and Cloudflare Worker secrets, which are encrypted at rest and masked in logs.
- Message text reaches the program through environment variables, never through shell command text.
- Dependencies are audited in CI with `cargo-deny` (advisories, licenses, sources).

## If a credential leaks

Revoke or change it first, then update the secrets: the DESCO password in the portal, the bot token with `/revoke` in @BotFather, the GitHub token in your developer settings, the Cloudflare token in the dashboard. Rewriting git history does not make a published secret safe again.
