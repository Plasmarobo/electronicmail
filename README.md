# electronicmail

A lean, fast email client written in Rust — Thunderbird-like, but leaner, with
intuitive **and** advanced search. Built on [egui](https://github.com/emilk/egui)
for a native, dependency-light UI and SQLite (FTS5) for compact local storage.

> **Status: working vertical slice.** Setup is friction-free: type your email
> address and the app auto-detects your provider (Gmail, Outlook, Yahoo, iCloud,
> and most IMAP hosts) and signs you in — Gmail with a one-click browser consent,
> others with just a password / app-password. You can connect **multiple
> accounts** and switch between them (or view all inboxes at once), sync your
> mail, read it with **safe rich-HTML rendering** (no remote images or trackers),
> run powerful searches, compose and send mail over SMTP, view/create Google
> Calendar events, and a hybrid spam filter (heuristics + a classifier that
> learns from your feedback) keeps junk out of the inbox.

## Goals & current status

| # | Goal | Status |
|---|------|--------|
| 1 | Configure any account easily (Gmail, custom) | ✅ Email-only wizard: type your address, autoconfig resolves servers + auth (Gmail OAuth one-click, others password/app-password) |
| 2 | Robust search and filtering | ✅ FTS5 + operator-aware parser (`""`, `+`, `-`, `from:`, `subject:`, `after:`…) |
| 3 | Show full email address (anti-fraud) | ✅ Sender address always shown in full, never just a display name |
| 4 | Strong spam filtering & security controls | ✅ Heuristics (SPF/DKIM/DMARC, spoofing, wording) + trainable Bayesian filter + allow/block lists |
| 5 | Fast, on-demand access | ✅ All network/DB work on a background thread; offline-first from local cache |
| 6 | Compact storage | ✅ Single SQLite file, WAL, deduplicated by UID |
| 7 | Google Calendar read/write | ✅ Calendar pane lists upcoming events and creates new ones via the granted scope |
| 8 | Send mail (compose/reply) | ✅ SMTP send via `lettre` (XOAUTH2 for Gmail, SASL LOGIN for password accounts) |
| 9 | Multiple accounts + switcher | ✅ Left sidebar lists every mailbox plus an "All inboxes" combined view; **＋ Add account** connects more without signing out |
| 10 | Unread-first inbox | ✅ Defaults to unread only, with a one-click **Show read too** toggle |
| 11 | Rich HTML rendering (safe) | ✅ Formatted headings/lists/links/bold; scripts, styles, images and all remote content are stripped — nothing is ever fetched |

## Architecture

```
main.rs        eframe entry point
app.rs         egui UI (account switcher, setup wizard, inbox list, search bar, message view, compose, calendar)
worker.rs      background thread; Command/Event channels; one IMAP session per account; ties everything together
auth.rs        Gmail OAuth2 loopback flow (PKCE + CSRF), token refresh; bundled client
autoconfig.rs  email-only setup: bundled OAuth client + provider/server discovery (ISPDB, .well-known)
imap_client.rs IMAP over TLS: XOAUTH2 + password login; fetch + parse (mail-parser)
smtp_client.rs SMTP send over TLS: XOAUTH2 (Gmail) + SASL LOGIN (password)
calendar.rs    Google Calendar API v3: list upcoming + create events
spam.rs        spam filtering: heuristics + Bayesian classifier + allow/block lists
htmlview.rs    sanitized rich-HTML renderer for egui (no remote/active content)
storage.rs     SQLite + FTS5 (upsert, recent, search, body, spam, Bayes model); synthetic id + per-account IMAP UID
search.rs      query parser -> FTS5 MATCH + structured filters
config.rs      TOML config (a list of accounts) + token/credential cache in the platform config dir
model.rs       shared data types
```

The UI thread never blocks: it sends `Command`s and receives `Event`s over
channels while the worker performs auth, IMAP and database work.

## Search syntax

Type naturally, or combine operators:

| Example | Meaning |
|---------|---------|
| `invoice budget` | both words (AND) |
| `"quarterly report"` | exact phrase |
| `-newsletter` | exclude a term |
| `+urgent` | required term |
| `from:alice` | sender name or address |
| `to:team@acme.com` | recipient |
| `subject:invoice` | subject only |
| `body:contract` | body only |
| `after:2024-01-01` / `before:2024-12-31` | date range |
| `is:unread` · `is:read` · `is:flagged` | message state |

Example: `from:billing subject:invoice -reminder after:2024-01-01`

## Multiple accounts

Connect as many mailboxes as you like. The left sidebar is the account switcher:

- **📥 All inboxes** — a single combined view across every account, sorted by
  date. Each row shows which mailbox it came from.
- **One account** — click any address to scope the inbox, search, and spam view
  to just that account. Outgoing mail then uses that account's identity.
- **＋ Add account** — opens the same email-only wizard to connect another
  account without signing out of the others; the new mailbox becomes active when
  it finishes connecting.

Under the hood each account keeps its own live IMAP session, and messages are
keyed by a synthetic id plus the per-mailbox IMAP UID, so identical UIDs from
different servers never collide.

## Unread-first inbox

The inbox defaults to **unread only**, so you see what needs attention first.
Use the **Show read too** toggle at the top of the message list to include
already-read mail (and **Unread only** to hide it again). The sender on every
row is shown in **bold** so it's easy to scan.

## Reading mail (safe rich HTML)

Messages are rendered with formatting — headings, bold/italic, bullet lists,
blockquotes and clickable links — while everything risky is stripped before
display. Scripts, stylesheets, `<img>`/`<iframe>`/`<object>` and any other
remote or active content are removed, and **nothing is ever fetched from the
network**, so tracking pixels can't phone home and remote exploits can't load.
Only `http(s):` and `mailto:` links are kept clickable (they open in your
browser); plain-text bodies are shown as-is when no HTML part is present.

## Composing & sending mail

Click **✉ Compose** in the top bar to open a message window. Enter one or more
recipients (comma-separated), a subject, and the body, then **Send**. Mail is
sent over SMTP with implicit TLS (port 465) or STARTTLS (e.g. 587):

- **Gmail / Workspace** authenticate with SASL **XOAUTH2** using the same
  OAuth token as IMAP — no password is stored.
- **Password / app-password** accounts authenticate with SASL **LOGIN** over
  TLS, using the SMTP host/port from the wizard.

## Google Calendar

When you sign in with Google, the calendar scope is granted automatically.
Switch to the **Calendar** tab to see your upcoming events (soonest first) from
your primary calendar. Click **＋ New event**, fill in a title, date
(`YYYY-MM-DD`), start time (`HH:MM`), duration, and optional location, then
**Create** — the event is written to your primary calendar and the list
refreshes. Times are interpreted in your local timezone.

## Spam filtering & anti-spoofing

Every message is scored as it arrives, combining two layers:

- **Heuristics** (no training needed) — reads the receiving server's
  `Authentication-Results` header and penalises **SPF/DKIM/DMARC** failures
  (the strongest spoofing signal), display names that hide a different address,
  mismatched `Reply-To` domains, all-caps/`!!!` subjects, spam vocabulary, and
  link-heavy bodies.
- **Bayesian classifier** — learns from your own feedback. Open a message and
  click **🚫 Mark as spam** or **✓ Not spam**; the model (stored locally in the
  mail database) retrains immediately and improves future scoring.

**Allow / block lists** override everything: **Block sender** files all of a
sender's mail as spam, **Allow sender** always keeps it in the inbox. Lists are
kept in `config.toml` and accept either full addresses or bare domains.

Filtered mail moves to the **Spam** tab (it never clutters the inbox or search
results). Open any message to see *why* it was flagged — the reasons are shown
in full so you stay in control. The spam threshold (default `0.5`) is
configurable in `config.toml` under `[spam]`.

## Setup wizard

On first launch a guided wizard connects an account in as few steps as
possible — most users only ever type their email address:

1. **Enter your email** — type `you@example.com` and press **Continue**.
2. **Auto-detection** — the app resolves your provider's IMAP/SMTP servers and
   sign-in method automatically. It checks a built-in table of common providers
   (Gmail, Outlook, Yahoo, iCloud, GMX, Fastmail, Zoho, Proton, …) and falls
   back to the Mozilla ISPDB and the domain's `autoconfig` / `.well-known`
   records.
3. **Sign in:**
   - **Gmail / Workspace** → your browser opens for Google consent (one click).
     No client id/secret, no Google Cloud console. A "reopen browser" button and
     a copyable link are provided as a fallback.
   - **Password providers** → just enter your password or app-password. If the
     provider needs an app-password, the wizard links to the right page.
   - **Unknown domain** → a manual form lets you enter the IMAP/SMTP host, port,
     username, and password yourself.

Credentials (a Google refresh token, or the IMAP password) are cached so later
launches sign in automatically.

### Gmail without setup

Gmail sign-in is seamless **when the binary ships with a bundled Google OAuth
client** (see below). If no client is bundled, the wizard automatically falls
back to Gmail's app-password path: it links you to
<https://myaccount.google.com/apppasswords>, you paste a 16-character
app-password, and you're in. Either way there is nothing to configure in a
developer console.

### Bundling a Google OAuth client (maintainers only)

To enable the one-click Google browser flow, compile with the app's OAuth
client credentials injected at build time:

```powershell
$env:EM_GOOGLE_CLIENT_ID  = "xxxxxx.apps.googleusercontent.com"
$env:EM_GOOGLE_CLIENT_SECRET = "your-client-secret"
cargo build --release
```

The client is a **Desktop app** OAuth client created once in Google Cloud
(enable the Gmail API + Google Calendar API). The loopback redirect
(`http://127.0.0.1:<random-port>`) is allowed automatically for Desktop clients.
PKCE protects the exchange, so embedding the client id/secret in the binary is
the standard, supported model (the same approach Thunderbird uses). Without
these variables the app still works for everyone via the app-password fallback.


## Build & run

```powershell
cargo run --release
```

Tests (search parser + FTS round-trip):

```powershell
cargo test
```

## Prebuilt releases & CI

Every push and pull request is built and tested on Linux, Windows and macOS by
GitHub Actions (`.github/workflows/ci.yml`), which also checks formatting
(`cargo fmt`) and lints (`cargo clippy`).

Tagged releases (`v*`) trigger `.github/workflows/release.yml`, which builds a
release binary for each desktop platform and attaches the archives to a GitHub
Release:

| Platform | Archive |
|----------|---------|
| Windows (x86-64) | `electronicmail-windows-x86_64.zip` |
| Linux (x86-64) | `electronicmail-linux-x86_64.tar.gz` |
| Linux (x86-64, portable) | `electronicmail-linux-x86_64.AppImage` |
| macOS (Apple silicon) | `electronicmail-macos-aarch64.tar.gz` |
| macOS (Intel) | `electronicmail-macos-x86_64.tar.gz` |

To cut a release, push a semver tag:

```powershell
git tag v0.2.0
git push origin v0.2.0
```

Download the archive for your platform from the **Releases** page, extract it,
and run the `electronicmail` binary. On Linux the AppImage is a single
self-contained file — `chmod +x electronicmail-linux-x86_64.AppImage` and run
it directly.


## Where data lives

- Config + cached refresh token: platform config dir (e.g.
  `%APPDATA%\electronicmail\electronicmail\config.toml` on Windows).
- Mail database: platform data dir, `mail.db` (SQLite, with WAL files).

## Security notes

- OAuth2 **Authorization Code + PKCE**; the `state` parameter is verified to
  prevent CSRF.
- The HTTP client used for token exchange disables redirects (SSRF hardening).
- All IMAP and SMTP traffic is over TLS; Gmail auth uses SASL **XOAUTH2** (no
  password stored). For password/app-password accounts the credential is cached
  locally in the config file — prefer provider app-passwords over your main
  password.
- The full sender address is always displayed to help you spot spoofing.
- Incoming mail is scored for SPF/DKIM/DMARC authentication failures and
  display-name spoofing; suspected spam is filtered out of the inbox with the
  reasons shown so you can verify them yourself.
- HTML mail is sanitized before rendering: scripts, styles and all remote
  content (images, iframes, objects) are stripped and never fetched, defeating
  tracking pixels and remote-content exploits.

## Roadmap (next slices)

- Reply/forward prefilling from the open message.
- OS keychain storage for cached credentials.
