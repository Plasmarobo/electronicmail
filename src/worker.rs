//! Background worker thread.
//!
//! All network and database work happens here so the egui UI thread never
//! blocks. The UI sends [`Command`]s and receives [`Event`]s over channels.
//!
//! The worker manages **multiple accounts**: one live IMAP [`Session`] per
//! account (keyed by email), a current view *scope* (`None` = all inboxes, or a
//! single account), and an unread-only toggle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::config::{self, AccountConfig, AppConfig};
use crate::imap_client::{self, Session};
use crate::model::{EmailBody, EmailSummary};
use crate::search;
use crate::spam;
use crate::storage::Store;
use crate::{auth, autoconfig, calendar, imap_client as imap_mod, smtp_client};

const INITIAL_FETCH: u32 = 50;
const VIEW_LIMIT: i64 = 300;
const CALENDAR_MAX: u32 = 50;

/// Auto-fetch poll cadence: start at five minutes and, when a poll turns up no
/// new mail, back off in five-minute steps up to half an hour. Any new mail (or
/// a manual sync) snaps the interval back to the minimum.
const POLL_MIN: Duration = Duration::from_secs(5 * 60);
const POLL_MAX: Duration = Duration::from_secs(30 * 60);
const POLL_STEP: Duration = Duration::from_secs(5 * 60);

/// Which authentication flow the user chose in the wizard.
pub enum AuthChoice {
    /// Google OAuth2 using the account's own ("bring your own client") OAuth
    /// client id/secret via the browser loopback flow.
    Google {
        client_id: String,
        client_secret: String,
    },
    /// Plain IMAP password / app-password.
    Password {
        imap_host: String,
        imap_port: u16,
        smtp_host: String,
        smtp_port: u16,
        username: String,
        password: String,
    },
}

/// Summary of a configured account, sent to the UI for the account list.
#[derive(Clone)]
pub struct AccountInfo {
    pub email: String,
    /// Whether this account supports Google Calendar (OAuth Google only).
    pub has_calendar: bool,
}

/// Details for an outgoing message.
pub struct Compose {
    /// The address to send from; selects which account's credentials are used.
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Details for a new calendar event (date/time pieces assembled by the UI).
pub struct NewEvent {
    pub summary: String,
    pub location: String,
    pub description: String,
    pub start_rfc3339: String,
    pub end_rfc3339: String,
}

/// Messages from the UI to the worker.
pub enum Command {
    /// Resolve IMAP/SMTP + auth settings from just an email address.
    Discover(String),
    Authenticate {
        email: String,
        choice: AuthChoice,
    },
    Sync,
    /// Internal: a background auto-fetch triggered by the poll timer (`None`,
    /// all accounts) or an IMAP IDLE push (`Some(email)`, one account).
    AutoSync(Option<String>),
    Search(String),
    OpenMessage(i64),
    SendMessage(Compose),
    LoadCalendar,
    CreateEvent(NewEvent),
    /// Load the spam folder.
    LoadSpam,
    /// Mark a message as spam (trains the classifier).
    MarkSpam(i64),
    /// Mark a message as not spam (trains the classifier).
    MarkNotSpam(i64),
    /// Add the message's sender to the block list and filter their mail.
    BlockSender(i64),
    /// Add the message's sender to the allow list and unfilter their mail.
    AllowSender(i64),
    /// Switch the inbox view to one account, or `None` for all inboxes.
    SelectScope(Option<String>),
    /// Toggle whether already-read messages are hidden.
    SetUnreadOnly(bool),
    /// Internal: the background update check found a newer release on GitHub.
    UpdateFound(crate::update::ReleaseInfo),
    /// Download the pending update and overwrite this executable.
    InstallUpdate,
    /// Start watching for downloaded OAuth client credentials (the wizard's
    /// auto-capture: scans Downloads for a `client_secret_*.json` file).
    WatchClientCredentials,
    /// Stop the OAuth client credential watch.
    StopClientCredentialsWatch,
}

/// Messages from the worker back to the UI.
pub enum Event {
    Status(String),
    /// The full list of configured accounts (drives the account switcher).
    Accounts(Vec<AccountInfo>),
    /// Settings discovered for an email address (provider, hosts, auth kind).
    Discovered(autoconfig::MailSettings),
    /// Autoconfig couldn't recognise the domain; the user must enter settings.
    DiscoveryFailed(String),
    /// The Google consent URL — surfaced so the UI can reopen it on demand.
    AuthUrl(String),
    /// Sign-in succeeded; `email` becomes the active account.
    Authenticated {
        email: String,
        /// Whether any account supports Google Calendar.
        calendar: bool,
    },
    NotAuthenticated,
    Messages(Vec<EmailSummary>),
    /// Unread (non-spam) message count per account email, for the mailbox
    /// list badges. Accounts with zero unread are omitted.
    UnreadCounts(HashMap<String, i64>),
    MessageBody(EmailBody),
    /// A foreground sync (on load or a manual refresh) started for an account.
    /// Drives the "Updating…" overlay. Periodic background syncs do *not* emit
    /// this so the overlay stays out of the way.
    SyncStarted(String),
    /// A foreground sync finished for an account (success or failure).
    SyncFinished(String),
    MessageSent,
    CalendarEvents(Vec<calendar::CalEvent>),
    CalendarEventCreated(calendar::CalEvent),
    /// Messages currently filtered as spam.
    SpamMessages(Vec<EmailSummary>),
    /// A newer release is available on GitHub.
    UpdateAvailable {
        version: String,
        notes: String,
    },
    /// The update was downloaded and installed; restart to apply it.
    UpdateInstalled,
    /// OAuth client credentials were auto-detected from the user's Downloads
    /// folder (the setup wizard's bring-your-own-client auto-capture).
    ClientCredentialsDetected {
        client_id: String,
        client_secret: String,
    },
    Error(String),
}

/// Spawn the worker and return the command sender + event receiver.
pub fn spawn(ctx: egui::Context) -> (Sender<Command>, Receiver<Event>) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Event>();

    // The worker keeps a sender so IDLE watcher threads can nudge it to sync.
    let self_tx = cmd_tx.clone();

    thread::spawn(move || {
        let store = match config::database_path().and_then(|p| Store::open(&p)) {
            Ok(s) => s,
            Err(e) => {
                let _ = evt_tx.send(Event::Error(format!("storage error: {e:#}")));
                return;
            }
        };
        let cfg = config::load().unwrap_or_default();
        let bayes = store.load_bayes().unwrap_or_default();

        let mut worker = Worker {
            cfg,
            store,
            bayes,
            sessions: HashMap::new(),
            scope: None,
            unread_only: true,
            events: evt_tx,
            ctx,
            cmd_tx: self_tx,
            idle_accounts: HashSet::new(),
            poll_interval: POLL_MIN,
            pending_update: None,
            cred_watch: None,
        };

        worker.startup();
        worker.run(cmd_rx);
    });

    (cmd_tx, evt_rx)
}

/// Scan a directory for a recently-downloaded Google OAuth client JSON
/// (`client_secret_*.json`) and return its `(client_id, client_secret)`. Picks
/// the newest matching file modified at or after `since`.
fn scan_for_client_credentials(
    dir: &std::path::Path,
    since: SystemTime,
) -> Option<(String, String)> {
    let mut newest: Option<(SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !(name.starts_with("client_secret") && name.ends_with(".json")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        // Skip implausibly large files — a real client JSON is tiny.
        if meta.len() > 64 * 1024 {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < since {
            continue;
        }
        if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
            newest = Some((modified, entry.path()));
        }
    }
    let text = std::fs::read_to_string(newest?.1).ok()?;
    auth::parse_oauth_client_json(&text)
}

struct Worker {
    cfg: AppConfig,
    store: Store,
    bayes: spam::BayesModel,
    /// One live IMAP session per account email.
    sessions: HashMap<String, Session>,
    /// Current view scope: `None` = all inboxes, `Some(email)` = one account.
    scope: Option<String>,
    /// Hide already-read messages (the default view).
    unread_only: bool,
    events: Sender<Event>,
    ctx: egui::Context,
    /// A clone of the command sender, handed to IDLE watcher threads so they
    /// can ask the worker to sync when the server reports new mail.
    cmd_tx: Sender<Command>,
    /// Accounts that already have an IMAP IDLE watcher running.
    idle_accounts: HashSet<String>,
    /// Current auto-fetch poll interval (adapts with mail volume).
    poll_interval: Duration,
    /// A newer release discovered by the background update check, awaiting the
    /// user's go-ahead to install.
    pending_update: Option<crate::update::ReleaseInfo>,
    /// Stop flag for the background OAuth-credential Downloads watcher.
    cred_watch: Option<Arc<AtomicBool>>,
}

impl Worker {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }

    fn status(&self, msg: impl Into<String>) {
        self.emit(Event::Status(msg.into()));
    }

    /// Whether any configured account uses Google (and thus has a calendar).
    fn any_calendar(&self) -> bool {
        self.cfg
            .accounts
            .iter()
            .any(|a| a.auth_method == config::AuthMethod::OAuthGoogle)
    }

    /// Broadcast the current account list to the UI.
    fn send_accounts(&self) {
        let list = self
            .cfg
            .accounts
            .iter()
            .map(|a| AccountInfo {
                email: a.email.clone(),
                has_calendar: a.auth_method == config::AuthMethod::OAuthGoogle,
            })
            .collect();
        self.emit(Event::Accounts(list));
    }

    /// The account used for actions (compose, calendar): the scoped account, or
    /// the first configured account when viewing all inboxes.
    fn active_account(&self) -> Option<AccountConfig> {
        let email = self
            .scope
            .clone()
            .or_else(|| self.cfg.accounts.first().map(|a| a.email.clone()))?;
        self.cfg.account(&email).cloned()
    }

    /// The best Google account for calendar actions (active if Google, else the
    /// first Google account configured).
    fn google_account(&self) -> Option<AccountConfig> {
        if let Some(acc) = self.active_account() {
            if acc.auth_method == config::AuthMethod::OAuthGoogle {
                return Some(acc);
            }
        }
        self.cfg
            .accounts
            .iter()
            .find(|a| a.auth_method == config::AuthMethod::OAuthGoogle)
            .cloned()
    }

    /// On launch, show any locally cached mail immediately, then restore a
    /// session for every account that has saved credentials.
    fn startup(&mut self) {
        // Check GitHub for a newer release in the background; the result (if
        // any) comes back as a `Command::UpdateFound`.
        crate::update::spawn_check(self.cmd_tx.clone());

        if self.cfg.accounts.is_empty() {
            self.emit(Event::NotAuthenticated);
            return;
        }

        self.send_accounts();
        // Flip the UI into the main view straight away using cached mail.
        let first = self.cfg.accounts[0].email.clone();
        self.emit(Event::Authenticated {
            email: first,
            calendar: self.any_calendar(),
        });
        self.current_messages();

        let accounts: Vec<AccountConfig> = self.cfg.accounts.clone();
        for account in accounts {
            let has_credentials = match account.auth_method {
                config::AuthMethod::OAuthGoogle => account.refresh_token.is_some(),
                config::AuthMethod::Password => account.password.is_some(),
            };
            if !has_credentials {
                continue;
            }
            self.status(format!("Connecting {}…", account.email));
            self.emit(Event::SyncStarted(account.email.clone()));
            match self.ensure_session(&account.email) {
                Ok(()) => {
                    self.sync_account(&account.email);
                }
                Err(e) => self.emit(Event::Error(format!(
                    "could not connect {}: {e:#}",
                    account.email
                ))),
            }
            self.emit(Event::SyncFinished(account.email.clone()));
        }
        self.current_messages();
        self.status("Up to date");
        self.start_idle_watchers();
    }

    fn run(&mut self, rx: Receiver<Command>) {
        let mut next_poll = Instant::now() + self.poll_interval;
        loop {
            let wait = next_poll.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(cmd) => {
                    // A manual sync or an IDLE-driven fetch resets the poll
                    // clock so we don't immediately poll again on top of it.
                    let reschedule = matches!(cmd, Command::Sync | Command::AutoSync(_));
                    self.handle(cmd);
                    if reschedule {
                        next_poll = Instant::now() + self.poll_interval;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.auto_sync(None);
                    next_poll = Instant::now() + self.poll_interval;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Discover(email) => self.discover(email),
            Command::Authenticate { email, choice } => self.authenticate(email, choice),
            Command::Sync => self.sync_all(),
            Command::AutoSync(which) => self.auto_sync(which),
            Command::Search(query) => self.search(query),
            Command::OpenMessage(uid) => self.open_message(uid),
            Command::SendMessage(compose) => self.send_message(compose),
            Command::LoadCalendar => self.load_calendar(),
            Command::CreateEvent(event) => self.create_event(event),
            Command::LoadSpam => self.send_spam(),
            Command::MarkSpam(uid) => self.mark_spam(uid, true),
            Command::MarkNotSpam(uid) => self.mark_spam(uid, false),
            Command::BlockSender(uid) => self.block_sender(uid, true),
            Command::AllowSender(uid) => self.block_sender(uid, false),
            Command::SelectScope(scope) => self.select_scope(scope),
            Command::SetUnreadOnly(unread) => {
                self.unread_only = unread;
                self.current_messages();
            }
            Command::UpdateFound(release) => self.on_update_found(release),
            Command::InstallUpdate => self.install_update(),
            Command::WatchClientCredentials => self.watch_client_credentials(),
            Command::StopClientCredentialsWatch => self.stop_client_credentials_watch(),
        }
    }

    /// Background auto-fetch. Syncs one account (an IDLE push) or all of them
    /// (the poll timer), refreshes the view when anything new arrives, and
    /// adapts the poll interval: new mail snaps back to [`POLL_MIN`], an empty
    /// poll backs off one [`POLL_STEP`] toward [`POLL_MAX`].
    fn auto_sync(&mut self, which: Option<String>) {
        let emails: Vec<String> = match which {
            Some(email) => vec![email],
            None => self.cfg.accounts.iter().map(|a| a.email.clone()).collect(),
        };
        if emails.is_empty() {
            return;
        }
        let mut new_mail = 0usize;
        let mut seen_changed = false;
        for email in &emails {
            let (count, changed) = self.sync_account(email);
            new_mail += count;
            seen_changed |= changed;
        }
        if new_mail > 0 {
            self.poll_interval = POLL_MIN;
            self.current_messages();
            self.send_spam();
            self.status(format!("{new_mail} new message(s)"));
        } else {
            self.poll_interval = (self.poll_interval + POLL_STEP).min(POLL_MAX);
            // Read-state may have changed on the server even with no new mail.
            if seen_changed {
                self.current_messages();
            }
        }
    }

    fn select_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        self.current_messages();
    }

    /// Start (or restart) the background watcher that auto-captures the OAuth
    /// client credentials the user downloads from the Google Cloud Console. It
    /// scans the Downloads folder for a `client_secret_*.json` file and, on
    /// finding a valid one, emits [`Event::ClientCredentialsDetected`].
    fn watch_client_credentials(&mut self) {
        self.stop_client_credentials_watch();

        let Some(downloads) =
            directories::UserDirs::new().and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
        else {
            return;
        };

        let flag = Arc::new(AtomicBool::new(true));
        self.cred_watch = Some(flag.clone());
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        // Accept files touched shortly before the watch began so a file the
        // user grabbed moments earlier is still picked up.
        let since = SystemTime::now() - Duration::from_secs(600);
        let deadline = Instant::now() + Duration::from_secs(15 * 60);

        thread::spawn(move || {
            while flag.load(Ordering::Relaxed) && Instant::now() < deadline {
                if let Some((client_id, client_secret)) =
                    scan_for_client_credentials(&downloads, since)
                {
                    let _ = events.send(Event::ClientCredentialsDetected {
                        client_id,
                        client_secret,
                    });
                    ctx.request_repaint();
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

    /// Signal the credential watcher (if any) to stop.
    fn stop_client_credentials_watch(&mut self) {
        if let Some(flag) = self.cred_watch.take() {
            flag.store(false, Ordering::Relaxed);
        }
    }

    /// Remember a release the background check found and tell the UI to offer it.
    fn on_update_found(&mut self, release: crate::update::ReleaseInfo) {
        self.emit(Event::UpdateAvailable {
            version: release.version.clone(),
            notes: release.notes.clone(),
        });
        self.pending_update = Some(release);
    }

    /// Download and apply the pending update, replacing this executable.
    fn install_update(&mut self) {
        let Some(release) = self.pending_update.clone() else {
            return;
        };
        self.status(format!("Downloading update {}…", release.version));
        match crate::update::install(&release) {
            Ok(()) => {
                self.pending_update = None;
                self.status("Update installed — restart to apply");
                self.emit(Event::UpdateInstalled);
            }
            Err(e) => self.emit(Event::Error(format!("Update failed: {e:#}"))),
        }
    }
    fn authenticate(&mut self, email: String, choice: AuthChoice) {
        match choice {
            AuthChoice::Google {
                client_id,
                client_secret,
            } => self.authenticate_google(email, client_id, client_secret),
            AuthChoice::Password {
                imap_host,
                imap_port,
                smtp_host,
                smtp_port,
                username,
                password,
            } => self.authenticate_password(
                email, imap_host, imap_port, smtp_host, smtp_port, username, password,
            ),
        }
    }

    /// Resolve account settings from an email address and report them back.
    fn discover(&self, email: String) {
        self.status("Looking up your provider…");
        match autoconfig::discover(&email) {
            Ok(settings) => {
                self.status(format!("Found settings for {}", settings.provider_name));
                self.emit(Event::Discovered(settings));
            }
            Err(e) => self.emit(Event::DiscoveryFailed(format!("{e:#}"))),
        }
    }

    fn authenticate_google(&mut self, email: String, client_id: String, client_secret: String) {
        let mut account =
            AccountConfig::google_byoc(email.clone(), client_id.clone(), client_secret.clone());
        self.status("Opening your browser to sign in with Google…");

        let events = self.events.clone();
        let ctx = self.ctx.clone();
        let tokens = match auth::interactive_login(&client_id, &client_secret, |url| {
            // Surface the URL so the UI can reopen it, then try to launch it.
            let _ = events.send(Event::AuthUrl(url.to_string()));
            ctx.request_repaint();
            let _ = webbrowser::open(url);
        }) {
            Ok(t) => t,
            Err(e) => {
                self.emit(Event::Error(format!("Google sign-in failed: {e:#}")));
                self.emit(Event::NotAuthenticated);
                return;
            }
        };

        account.refresh_token = tokens.refresh_token.clone();

        self.status("Connecting to Gmail…");
        match imap_client::connect(
            &account.imap_host,
            account.imap_port,
            &account.email,
            &tokens.access_token,
        ) {
            Ok(session) => {
                self.sessions.insert(account.email.clone(), session);
                self.finish_sign_in(account);
            }
            Err(e) => {
                self.emit(Event::Error(format!("IMAP connect failed: {e:#}")));
                self.emit(Event::NotAuthenticated);
            }
        }
    }

    fn authenticate_password(
        &mut self,
        email: String,
        imap_host: String,
        imap_port: u16,
        smtp_host: String,
        smtp_port: u16,
        username: String,
        password: String,
    ) {
        let account = AccountConfig::password(
            email.clone(),
            imap_host.clone(),
            imap_port,
            smtp_host,
            smtp_port,
            username.clone(),
            password.clone(),
        );
        self.status(format!("Connecting to {imap_host}…"));

        match imap_client::connect_password(&imap_host, imap_port, &username, &password) {
            Ok(session) => {
                self.sessions.insert(account.email.clone(), session);
                self.finish_sign_in(account);
            }
            Err(e) => {
                self.emit(Event::Error(format!("Sign-in failed: {e:#}")));
                self.emit(Event::NotAuthenticated);
            }
        }
    }

    /// Persist a newly-signed-in account, make it the active scope, and sync it.
    fn finish_sign_in(&mut self, account: AccountConfig) {
        let email = account.email.clone();
        self.cfg.upsert_account(account);
        if let Err(e) = config::save(&self.cfg) {
            self.emit(Event::Error(format!("could not save config: {e:#}")));
        }
        self.send_accounts();
        self.scope = Some(email.clone());
        self.emit(Event::Authenticated {
            email: email.clone(),
            calendar: self.any_calendar(),
        });
        self.sync_account(&email);
        self.current_messages();
        self.start_idle_watchers();
    }

    /// Make sure we have a live IMAP session for `email`, refreshing the access
    /// token and reconnecting if necessary.
    fn ensure_session(&mut self, email: &str) -> anyhow::Result<()> {
        if self.sessions.contains_key(email) {
            return Ok(());
        }
        let account = self
            .cfg
            .account(email)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such account: {email}"))?;

        let session = match account.auth_method {
            config::AuthMethod::OAuthGoogle => {
                let refresh_token = account
                    .refresh_token
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
                let (client_id, client_secret) = account.effective_client();
                let tokens = auth::refresh(&client_id, &client_secret, &refresh_token)?;
                // Persist a rotated refresh token if Google issued one.
                if tokens.refresh_token != account.refresh_token {
                    if let Some(acc) = self.cfg.account_mut(email) {
                        acc.refresh_token = tokens.refresh_token.clone();
                    }
                    let _ = config::save(&self.cfg);
                }
                imap_client::connect(
                    &account.imap_host,
                    account.imap_port,
                    &account.email,
                    &tokens.access_token,
                )?
            }
            config::AuthMethod::Password => {
                let password = account
                    .password
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no stored password"))?;
                imap_client::connect_password(
                    &account.imap_host,
                    account.imap_port,
                    &account.username,
                    &password,
                )?
            }
        };
        self.sessions.insert(email.to_string(), session);
        Ok(())
    }

    /// Sync every configured account, then refresh the current view.
    fn sync_all(&mut self) {
        let emails: Vec<String> = self.cfg.accounts.iter().map(|a| a.email.clone()).collect();
        for email in emails {
            self.status(format!("Syncing {email}…"));
            self.emit(Event::SyncStarted(email.clone()));
            self.sync_account(&email);
            self.emit(Event::SyncFinished(email.clone()));
        }
        self.current_messages();
        self.status("Up to date");
    }

    /// Start an IMAP IDLE watcher for every account that has credentials and
    /// isn't already being watched, so new mail shows up without waiting for
    /// the next poll. Servers without IDLE simply fall back to polling.
    fn start_idle_watchers(&mut self) {
        let accounts: Vec<AccountConfig> = self.cfg.accounts.clone();
        for account in accounts {
            if self.idle_accounts.contains(&account.email) {
                continue;
            }
            let has_credentials = match account.auth_method {
                config::AuthMethod::OAuthGoogle => account.refresh_token.is_some(),
                config::AuthMethod::Password => account.password.is_some(),
            };
            if !has_credentials {
                continue;
            }
            self.idle_accounts.insert(account.email.clone());
            crate::idle::spawn(account, self.cmd_tx.clone(), self.ctx.clone());
        }
    }

    /// Fetch new mail for a single account and persist it, then reconcile
    /// read-state with the server. Returns the number of newly fetched messages
    /// and whether any existing message's read-state changed.
    fn sync_account(&mut self, email: &str) -> (usize, bool) {
        let Some(account) = self.cfg.account(email).cloned() else {
            return (0, false);
        };
        if let Err(e) = self.ensure_session(email) {
            self.emit(Event::Error(format!("connect failed for {email}: {e:#}")));
            return (0, false);
        }

        let since = match self.store.max_uid(&account.email) {
            Ok(v) => v,
            Err(e) => {
                self.emit(Event::Error(format!("db error: {e:#}")));
                return (0, false);
            }
        };

        let session = self.sessions.get_mut(email).expect("session ensured above");
        let fetched = match imap_mod::fetch_inbox(session, &account.email, since, INITIAL_FETCH) {
            Ok(v) => v,
            Err(e) => {
                // Session may be stale; drop it so the next sync reconnects.
                self.sessions.remove(email);
                self.emit(Event::Error(format!("fetch failed for {email}: {e:#}")));
                return (0, false);
            }
        };

        let count = fetched.len();
        for mut m in fetched {
            self.classify(&account, &mut m);
            if let Err(e) = self.store.upsert(&m) {
                self.emit(Event::Error(format!("store error: {e:#}")));
            }
        }

        // Pull read/unread changes made on the server (e.g. another device).
        let seen_changed = self.reconcile_seen(&account.email);
        (count, seen_changed)
    }

    /// Pull the server's `\Seen` flags for messages we already store and apply
    /// any differences to the local DB. Together with the immediate push when a
    /// message is opened, this keeps read-state in sync bidirectionally so a
    /// message read (or marked unread) on another device is reflected here.
    /// Returns whether any local read-state actually changed.
    fn reconcile_seen(&mut self, email: &str) -> bool {
        let local = match self.store.seen_map(email) {
            Ok(m) => m,
            Err(e) => {
                self.emit(Event::Error(format!("db error: {e:#}")));
                return false;
            }
        };
        if local.is_empty() {
            return false;
        }
        let uids: Vec<i64> = local.keys().copied().collect();

        // Scope the mutable session borrow so the DB writes below can borrow
        // `self` again.
        let server = {
            let Some(session) = self.sessions.get_mut(email) else {
                return false;
            };
            match imap_mod::fetch_flags(session, &uids) {
                Ok(s) => s,
                Err(e) => {
                    self.sessions.remove(email);
                    self.emit(Event::Error(format!("flag sync failed for {email}: {e:#}")));
                    return false;
                }
            }
        };

        let mut changed = false;
        for (uid, &local_seen) in &local {
            if let Some(&server_seen) = server.get(uid) {
                if server_seen != local_seen {
                    match self.store.set_seen_by_imap_uid(email, *uid, server_seen) {
                        Ok(()) => changed = true,
                        Err(e) => self.emit(Event::Error(format!("db error: {e:#}"))),
                    }
                }
            }
        }
        changed
    }

    /// Run the spam classifier over a freshly fetched message, filling in its
    /// verdict fields before it is persisted.
    fn classify(&self, account: &AccountConfig, m: &mut crate::storage::StoredMessage) {
        let signals = spam::Signals {
            from_name: m.from_name.clone(),
            from_addr: m.from_addr.clone(),
            reply_to: m.reply_to.clone(),
            to_addrs: m.to_addrs.clone(),
            subject: m.subject.clone(),
            body: m.body_text.clone(),
            auth_results: m.auth_results.clone(),
            account: account.email.clone(),
        };
        let verdict = spam::classify(
            &signals,
            &self.bayes,
            &self.cfg.spam.block_list,
            &self.cfg.spam.allow_list,
            self.cfg.spam.threshold,
        );
        m.spam_score = verdict.score;
        m.is_spam = verdict.is_spam;
        m.spam_reasons = verdict.reasons.join("; ");
    }

    fn search(&mut self, query: String) {
        if query.trim().is_empty() {
            self.current_messages();
            return;
        }
        let parsed = search::parse(&query);
        match self
            .store
            .search(self.scope.as_deref(), &parsed, VIEW_LIMIT)
        {
            Ok(rows) => {
                self.status(format!("{} result(s)", rows.len()));
                self.emit(Event::Messages(rows));
            }
            Err(e) => self.emit(Event::Error(format!("search error: {e:#}"))),
        }
    }

    /// Emit the inbox list for the current scope + unread filter.
    fn current_messages(&self) {
        match self
            .store
            .recent(self.scope.as_deref(), self.unread_only, VIEW_LIMIT)
        {
            Ok(rows) => self.emit(Event::Messages(rows)),
            Err(e) => self.emit(Event::Error(format!("load error: {e:#}"))),
        }
        self.send_unread_counts();
    }

    /// Emit the per-account unread badge counts.
    fn send_unread_counts(&self) {
        match self.store.unread_counts() {
            Ok(counts) => self.emit(Event::UnreadCounts(counts)),
            Err(e) => self.emit(Event::Error(format!("unread count error: {e:#}"))),
        }
    }

    /// Send the spam folder contents for the current scope.
    fn send_spam(&self) {
        match self.store.spam(self.scope.as_deref(), VIEW_LIMIT) {
            Ok(rows) => self.emit(Event::SpamMessages(rows)),
            Err(e) => self.emit(Event::Error(format!("spam load error: {e:#}"))),
        }
    }

    /// Refresh both the inbox and spam lists after a classification change.
    fn refresh_lists(&self) {
        self.current_messages();
        self.send_spam();
    }

    /// Mark a message as spam (or not) and teach the Bayesian classifier.
    fn mark_spam(&mut self, uid: i64, is_spam: bool) {
        if let Ok(Some(text)) = self.store.training_text(uid) {
            self.bayes.train(&text, is_spam);
            if let Err(e) = self.store.save_bayes(&self.bayes) {
                self.emit(Event::Error(format!("could not save spam model: {e:#}")));
            }
        }
        let (score, reason) = if is_spam {
            (1.0, "Marked as spam by you")
        } else {
            (0.0, "Marked as not spam by you")
        };
        if let Err(e) = self.store.set_spam(uid, is_spam, score, reason) {
            self.emit(Event::Error(format!("could not update message: {e:#}")));
        }
        self.status(if is_spam {
            "Moved to spam — classifier updated"
        } else {
            "Restored to inbox — classifier updated"
        });
        self.refresh_lists();
    }

    /// Add (or remove) a sender from the block list and reclassify their mail
    /// across every account (the block/allow lists are global).
    fn block_sender(&mut self, uid: i64, block: bool) {
        let addr = match self.store.from_addr(uid) {
            Ok(Some(a)) if !a.trim().is_empty() => a.trim().to_ascii_lowercase(),
            _ => {
                self.emit(Event::Error("no sender address on that message".into()));
                return;
            }
        };

        {
            let lists = &mut self.cfg.spam;
            if block {
                lists.allow_list.retain(|e| e != &addr);
                if !lists.block_list.contains(&addr) {
                    lists.block_list.push(addr.clone());
                }
            } else {
                lists.block_list.retain(|e| e != &addr);
                if !lists.allow_list.contains(&addr) {
                    lists.allow_list.push(addr.clone());
                }
            }
        }
        if let Err(e) = config::save(&self.cfg) {
            self.emit(Event::Error(format!("could not save config: {e:#}")));
        }

        let reason = if block {
            "Sender is on your block list"
        } else {
            "Sender is on your allow list"
        };
        if let Err(e) = self.store.set_spam_for_sender_all(&addr, block, reason) {
            self.emit(Event::Error(format!("could not update messages: {e:#}")));
        }
        self.status(if block {
            format!("Blocked {addr}")
        } else {
            format!("Allowed {addr}")
        });
        self.refresh_lists();
    }

    fn open_message(&mut self, uid: i64) {
        match self.store.body(uid) {
            Ok(Some((text, html))) => {
                self.emit(Event::MessageBody(EmailBody { uid, text, html }));
                // Opening a message marks it read — locally and, best-effort,
                // on the server so the state is consistent across devices.
                match self.store.mark_seen(uid) {
                    Ok(Some((account, imap_uid))) => {
                        if self.ensure_session(&account).is_ok() {
                            if let Some(session) = self.sessions.get_mut(&account) {
                                let _ = imap_mod::mark_seen(session, imap_uid);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => self.emit(Event::Status(format!("couldn't mark read: {e:#}"))),
                }
                // Reading a message lowers its account's unread badge.
                self.send_unread_counts();
            }
            Ok(None) => self.emit(Event::Error("message body not found".into())),
            Err(e) => self.emit(Event::Error(format!("body load error: {e:#}"))),
        }
    }

    /// Mint a fresh Google OAuth2 access token for a specific account from its
    /// stored refresh token. Access tokens are short-lived, so we refresh on
    /// demand before each SMTP/Calendar operation rather than caching expiry.
    fn google_access_token(&mut self, email: &str) -> anyhow::Result<String> {
        let account = self
            .cfg
            .account(email)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such account: {email}"))?;
        let refresh_token = account
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("not signed in with Google"))?;
        let (client_id, client_secret) = account.effective_client();
        let tokens = auth::refresh(&client_id, &client_secret, &refresh_token)?;
        if tokens.refresh_token != account.refresh_token {
            if let Some(acc) = self.cfg.account_mut(email) {
                acc.refresh_token = tokens.refresh_token.clone();
            }
            let _ = config::save(&self.cfg);
        }
        Ok(tokens.access_token)
    }

    fn send_message(&mut self, compose: Compose) {
        // Send from the explicitly chosen address when it matches a configured
        // account, otherwise fall back to the active account.
        let account = self
            .cfg
            .account(&compose.from)
            .cloned()
            .or_else(|| self.active_account());
        let Some(account) = account else {
            self.emit(Event::Error("no account configured".into()));
            return;
        };
        self.status("Sending…");
        let body_html = crate::format::to_html(&compose.body);
        let outgoing = smtp_client::Outgoing {
            from: account.email.clone(),
            to: compose.to,
            subject: compose.subject,
            body: compose.body,
            body_html: Some(body_html),
        };

        let result = match account.auth_method {
            config::AuthMethod::OAuthGoogle => match self.google_access_token(&account.email) {
                Ok(token) => smtp_client::send_oauth(
                    &account.smtp_host,
                    account.smtp_port,
                    &account.email,
                    &token,
                    &outgoing,
                ),
                Err(e) => Err(e),
            },
            config::AuthMethod::Password => match account.password.clone() {
                Some(password) => smtp_client::send_password(
                    &account.smtp_host,
                    account.smtp_port,
                    &account.username,
                    &password,
                    &outgoing,
                ),
                None => Err(anyhow::anyhow!("no stored password")),
            },
        };

        match result {
            Ok(()) => {
                self.status("Message sent");
                self.emit(Event::MessageSent);
            }
            Err(e) => self.emit(Event::Error(format!("send failed: {e:#}"))),
        }
    }

    fn load_calendar(&mut self) {
        let Some(account) = self.google_account() else {
            self.emit(Event::Error("no Google account for calendar".into()));
            return;
        };
        self.status("Loading calendar…");
        let token = match self.google_access_token(&account.email) {
            Ok(t) => t,
            Err(e) => {
                self.emit(Event::Error(format!("calendar auth failed: {e:#}")));
                return;
            }
        };
        let now = chrono::Utc::now().to_rfc3339();
        match calendar::list_upcoming(&token, &now, CALENDAR_MAX) {
            Ok(events) => {
                self.status(format!("{} upcoming event(s)", events.len()));
                self.emit(Event::CalendarEvents(events));
            }
            Err(e) => self.emit(Event::Error(format!("calendar load failed: {e:#}"))),
        }
    }

    fn create_event(&mut self, event: NewEvent) {
        let Some(account) = self.google_account() else {
            self.emit(Event::Error("no Google account for calendar".into()));
            return;
        };
        self.status("Creating event…");
        let token = match self.google_access_token(&account.email) {
            Ok(t) => t,
            Err(e) => {
                self.emit(Event::Error(format!("calendar auth failed: {e:#}")));
                return;
            }
        };
        let new_event = calendar::NewEvent {
            summary: event.summary,
            description: event.description,
            location: event.location,
            start_rfc3339: event.start_rfc3339,
            end_rfc3339: event.end_rfc3339,
        };
        match calendar::create_event(&token, &new_event) {
            Ok(created) => {
                self.status("Event created");
                self.emit(Event::CalendarEventCreated(created));
            }
            Err(e) => self.emit(Event::Error(format!("create event failed: {e:#}"))),
        }
    }
}
