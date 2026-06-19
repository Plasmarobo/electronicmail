//! IMAP IDLE push: a per-account watcher thread that blocks on the server's
//! IDLE command and nudges the worker to sync the instant the mailbox changes.
//!
//! Each watcher uses its own dedicated IMAP connection (separate from the
//! worker's fetch session) so it can block indefinitely without stalling the
//! UI. If the server doesn't advertise IDLE the watcher exits quietly and the
//! worker's adaptive poll loop covers the account instead.

use std::ops::ControlFlow;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::auth;
use crate::config::{AccountConfig, AuthMethod};
use crate::imap_client::{self, Session};
use crate::worker::Command;

/// Re-issue IDLE well within Gmail's 29-minute server limit (RFC 2177).
const IDLE_TIMEOUT: Duration = Duration::from_secs(9 * 60);
/// Reconnect before an OAuth access token (~1 hour) can expire mid-IDLE.
const SESSION_MAX_AGE: Duration = Duration::from_secs(45 * 60);
/// Wait this long between connection cycles (and after errors) to avoid a
/// tight reconnect loop; the poll loop keeps mail flowing in the meantime.
const RECONNECT_DELAY: Duration = Duration::from_secs(60);

/// Spawn a background thread that watches one account's INBOX with IMAP IDLE,
/// sending `Command::AutoSync(Some(email))` whenever the mailbox changes.
pub fn spawn(account: AccountConfig, cmd_tx: Sender<Command>, ctx: egui::Context) {
    thread::spawn(move || watch(account, cmd_tx, ctx));
}

fn watch(account: AccountConfig, cmd_tx: Sender<Command>, ctx: egui::Context) {
    loop {
        if let Ok(mut session) = connect(&account) {
            // If the server can't push, stop — the worker's poller handles it.
            if let Ok(caps) = session.capabilities()
                && !caps.has_str("IDLE")
            {
                return;
            }
            if let ControlFlow::Break(()) = run_idle(&mut session, &account, &cmd_tx, &ctx) {
                // The UI is gone; nothing left to notify.
                return;
            }
            let _ = session.logout();
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

/// Block on IDLE until the mailbox changes, then nudge the worker. Returns
/// `Break` when the command channel is closed (app exiting), or `Continue`
/// when the connection should be refreshed.
fn run_idle(
    session: &mut Session,
    account: &AccountConfig,
    cmd_tx: &Sender<Command>,
    ctx: &egui::Context,
) -> ControlFlow<()> {
    if session.select("INBOX").is_err() {
        return ControlFlow::Continue(());
    }
    let started = Instant::now();
    loop {
        // Refresh the whole connection periodically so a stale OAuth token
        // never lingers on an open IDLE channel.
        if started.elapsed() >= SESSION_MAX_AGE {
            return ControlFlow::Continue(());
        }
        match session
            .idle()
            .and_then(|handle| handle.wait_with_timeout(IDLE_TIMEOUT))
        {
            Ok(imap::extensions::idle::WaitOutcome::MailboxChanged) => {
                if cmd_tx
                    .send(Command::AutoSync(Some(account.email.clone())))
                    .is_err()
                {
                    return ControlFlow::Break(());
                }
                ctx.request_repaint();
            }
            // Timed out: just re-issue IDLE to keep the connection alive.
            Ok(imap::extensions::idle::WaitOutcome::TimedOut) => {}
            // Connection hiccup: drop out and reconnect.
            Err(_) => return ControlFlow::Continue(()),
        }
    }
}

/// Open a fresh authenticated IMAP session for the account.
fn connect(account: &AccountConfig) -> anyhow::Result<Session> {
    match account.auth_method {
        AuthMethod::OAuthGoogle => {
            let refresh = account
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
            let (client_id, client_secret) = account.effective_client();
            let tokens = auth::refresh(&client_id, &client_secret, refresh)?;
            imap_client::connect(
                &account.imap_host,
                account.imap_port,
                &account.email,
                &tokens.access_token,
            )
        }
        AuthMethod::Password => {
            let password = account
                .password
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("no stored password"))?;
            imap_client::connect_password(
                &account.imap_host,
                account.imap_port,
                &account.username,
                password,
            )
        }
    }
}
