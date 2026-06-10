//! Background worker thread.
//!
//! All network and database work happens here so the egui UI thread never
//! blocks. The UI sends [`Command`]s and receives [`Event`]s over channels.
//!
//! The worker manages **multiple accounts**: one live IMAP [`Session`] per
//! account (keyed by email), a current view *scope* (`None` = all inboxes, or a
//! single account), and an unread-only toggle.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

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
    /// Seamless Google OAuth2 with the bundled client (browser loopback flow).
    Google,
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
    MessageSent,
    CalendarEvents(Vec<calendar::CalEvent>),
    CalendarEventCreated(calendar::CalEvent),
    /// Messages currently filtered as spam.
    SpamMessages(Vec<EmailSummary>),
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
        };

        worker.startup();
        worker.run(cmd_rx);
    });

    (cmd_tx, evt_rx)
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
            match self.ensure_session(&account.email) {
                Ok(()) => {
                    self.sync_account(&account.email);
                }
                Err(e) => self.emit(Event::Error(format!(
                    "could not connect {}: {e:#}",
                    account.email
                ))),
            }
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
        for email in &emails {
            new_mail += self.sync_account(email);
        }
        if new_mail > 0 {
            self.poll_interval = POLL_MIN;
            self.current_messages();
            self.send_spam();
            self.status(format!("{new_mail} new message(s)"));
        } else {
            self.poll_interval = (self.poll_interval + POLL_STEP).min(POLL_MAX);
        }
    }

    fn select_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        self.current_messages();
    }

    fn authenticate(&mut self, email: String, choice: AuthChoice) {
        match choice {
            AuthChoice::Google => self.authenticate_google(email),
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

    fn authenticate_google(&mut self, email: String) {
        let mut account = AccountConfig::google(email.clone());
        self.status("Opening your browser to sign in with Google…");

        let events = self.events.clone();
        let ctx = self.ctx.clone();
        let tokens = match auth::google_login(|url| {
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
                let tokens = auth::google_refresh(&refresh_token)?;
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
            self.sync_account(&email);
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

    /// Fetch new mail for a single account and persist it. Returns the number
    /// of newly fetched messages.
    fn sync_account(&mut self, email: &str) -> usize {
        let Some(account) = self.cfg.account(email).cloned() else {
            return 0;
        };
        if let Err(e) = self.ensure_session(email) {
            self.emit(Event::Error(format!("connect failed for {email}: {e:#}")));
            return 0;
        }

        let since = match self.store.max_uid(&account.email) {
            Ok(v) => v,
            Err(e) => {
                self.emit(Event::Error(format!("db error: {e:#}")));
                return 0;
            }
        };

        let session = self.sessions.get_mut(email).expect("session ensured above");
        let fetched = match imap_mod::fetch_inbox(session, &account.email, since, INITIAL_FETCH) {
            Ok(v) => v,
            Err(e) => {
                // Session may be stale; drop it so the next sync reconnects.
                self.sessions.remove(email);
                self.emit(Event::Error(format!("fetch failed for {email}: {e:#}")));
                return 0;
            }
        };

        let count = fetched.len();
        for mut m in fetched {
            self.classify(&account, &mut m);
            if let Err(e) = self.store.upsert(&m) {
                self.emit(Event::Error(format!("store error: {e:#}")));
            }
        }
        count
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
        let tokens = auth::google_refresh(&refresh_token)?;
        if tokens.refresh_token != account.refresh_token {
            if let Some(acc) = self.cfg.account_mut(email) {
                acc.refresh_token = tokens.refresh_token.clone();
            }
            let _ = config::save(&self.cfg);
        }
        Ok(tokens.access_token)
    }

    fn send_message(&mut self, compose: Compose) {
        let Some(account) = self.active_account() else {
            self.emit(Event::Error("no account configured".into()));
            return;
        };
        self.status("Sending…");
        let outgoing = smtp_client::Outgoing {
            from: account.email.clone(),
            to: compose.to,
            subject: compose.subject,
            body: compose.body,
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
