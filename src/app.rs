//! egui front-end: account setup, inbox list, search bar and message view.

use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

use crate::model::{EmailBody, EmailSummary};
use crate::worker::{self, AuthChoice, Command, Event};

/// Which step of the account-setup wizard is showing.
#[derive(Clone, Copy, PartialEq)]
enum WizardStep {
    /// Enter the email address (the only thing most users ever type).
    Email,
    /// Looking up the provider's settings.
    Discovering,
    /// Provider known — just collect the password / app-password.
    Password,
    /// Provider not recognised — collect server settings manually.
    Manual,
    /// Gmail with a bring-your-own OAuth client — guided credential setup.
    GoogleSetup,
    /// Authentication in progress (browser or IMAP login).
    Connecting,
}

/// Top-level view once signed in.
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Mail,
    Spam,
    Calendar,
}

pub struct App {
    cmd_tx: Sender<Command>,
    evt_rx: Receiver<Event>,

    authenticated: bool,
    account_email: String,
    status: String,
    error: Option<String>,
    /// True for Google accounts that support the calendar.
    has_calendar: bool,
    view: ViewMode,
    /// Configured accounts (drives the switcher); empty until the first sync.
    accounts: Vec<worker::AccountInfo>,
    /// Unread (non-spam) message count per account email, for the badges.
    unread_counts: HashMap<String, i64>,
    /// Active mailbox scope: `None` = combined "All inboxes" view.
    scope: Option<String>,
    /// Hide already-read messages (the default view).
    unread_only: bool,
    /// True while the setup wizard is being used to add an extra account.
    adding_account: bool,

    /// Accounts whose mail is currently being synced in the foreground (on load
    /// or a manual refresh). Drives the "Updating…" overlay. Background polls
    /// are deliberately excluded so the overlay never flashes on its own.
    syncing: std::collections::HashSet<String>,
    /// Set when the user dismisses the sync overlay with its ✕; cleared the next
    /// time a foreground sync starts.
    sync_overlay_dismissed: bool,

    // Setup wizard
    wizard: WizardStep,
    /// Settings resolved from the email address (provider, hosts, auth kind).
    discovered: Option<crate::autoconfig::MailSettings>,
    form_email: String,
    form_imap_host: String,
    form_imap_port: String,
    form_smtp_host: String,
    form_smtp_port: String,
    form_username: String,
    form_password: String,
    /// Bring-your-own-client OAuth credentials collected by the Google wizard.
    form_client_id: String,
    form_client_secret: String,
    /// Scratch box for pasting the downloaded client JSON / id+secret.
    form_client_paste: String,
    /// True while the worker is watching the Downloads folder for a downloaded
    /// OAuth client credentials file.
    cred_watch_active: bool,
    /// Google consent URL surfaced by the worker so we can reopen it.
    auth_url: Option<String>,
    show_help: bool,

    // Mail
    search_text: String,
    messages: Vec<EmailSummary>,
    selected: Option<i64>,
    body: Option<EmailBody>,

    // Spam
    spam_messages: Vec<EmailSummary>,
    spam_loaded: bool,

    // Compose
    compose_open: bool,
    /// The chosen From address (selects which account a message is sent as).
    compose_from: String,
    compose_to: String,
    compose_subject: String,
    compose_body: String,
    sending: bool,

    // Calendar
    calendar_events: Vec<crate::calendar::CalEvent>,
    calendar_loaded: bool,
    new_event_open: bool,
    ev_summary: String,
    ev_location: String,
    ev_date: String,
    ev_start_time: String,
    ev_duration_min: String,

    // Self-update
    /// Version string of an available update, once the check reports one.
    update_version: Option<String>,
    /// Markdown release notes for the available update.
    update_notes: String,
    /// Whether the update prompt window is currently shown.
    show_update: bool,
    /// True while the update is downloading/installing.
    updating: bool,
    /// True once the update has been written; the app must restart to apply it.
    update_installed: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);
        let (cmd_tx, evt_rx) = worker::spawn(cc.egui_ctx.clone());
        Self {
            cmd_tx,
            evt_rx,
            authenticated: false,
            account_email: String::new(),
            status: "Starting…".to_string(),
            error: None,
            has_calendar: false,
            view: ViewMode::Mail,
            accounts: Vec::new(),
            unread_counts: HashMap::new(),
            scope: None,
            unread_only: true,
            adding_account: false,
            syncing: std::collections::HashSet::new(),
            sync_overlay_dismissed: false,
            wizard: WizardStep::Email,
            discovered: None,
            form_email: String::new(),
            form_imap_host: String::new(),
            form_imap_port: "993".to_string(),
            form_smtp_host: String::new(),
            form_smtp_port: "465".to_string(),
            form_username: String::new(),
            form_password: String::new(),
            form_client_id: String::new(),
            form_client_secret: String::new(),
            form_client_paste: String::new(),
            cred_watch_active: false,
            auth_url: None,
            show_help: false,
            search_text: String::new(),
            messages: Vec::new(),
            spam_messages: Vec::new(),
            spam_loaded: false,
            selected: None,
            body: None,
            compose_open: false,
            compose_from: String::new(),
            compose_to: String::new(),
            compose_subject: String::new(),
            compose_body: String::new(),
            sending: false,
            calendar_events: Vec::new(),
            calendar_loaded: false,
            new_event_open: false,
            ev_summary: String::new(),
            ev_location: String::new(),
            ev_date: String::new(),
            ev_start_time: "09:00".to_string(),
            ev_duration_min: "60".to_string(),
            update_version: None,
            update_notes: String::new(),
            show_update: false,
            updating: false,
            update_installed: false,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.evt_rx.try_recv() {
            match event {
                Event::Status(s) => self.status = s,
                Event::Discovered(settings) => self.on_discovered(settings),
                Event::DiscoveryFailed(msg) => {
                    // Fall back to manual server entry, seeded with sensible defaults.
                    self.status = "Couldn't detect your provider automatically.".to_string();
                    self.error = Some(msg);
                    self.form_username = self.form_email.clone();
                    if self.form_imap_port.is_empty() {
                        self.form_imap_port = "993".to_string();
                    }
                    if self.form_smtp_port.is_empty() {
                        self.form_smtp_port = "465".to_string();
                    }
                    self.wizard = WizardStep::Manual;
                }
                Event::Accounts(list) => {
                    self.has_calendar = list.iter().any(|a| a.has_calendar);
                    if !list.is_empty() {
                        self.authenticated = true;
                    }
                    self.accounts = list;
                }
                Event::AuthUrl(url) => self.auth_url = Some(url),
                Event::Authenticated { email, calendar } => {
                    self.authenticated = true;
                    self.account_email = email.clone();
                    self.has_calendar = calendar;
                    self.error = None;
                    self.auth_url = None;
                    // Clear any secret sitting in the form.
                    self.form_password.clear();
                    // A freshly added account: focus its inbox and leave the
                    // wizard. (Startup sign-in keeps the combined view.)
                    if self.adding_account {
                        self.scope = Some(email);
                        self.adding_account = false;
                    }
                    // Stop any credential watcher and wipe captured secrets.
                    if self.cred_watch_active {
                        self.send(Command::StopClientCredentialsWatch);
                        self.cred_watch_active = false;
                    }
                    self.form_client_id.clear();
                    self.form_client_secret.clear();
                    self.form_client_paste.clear();
                    self.wizard = WizardStep::Email;
                }
                Event::NotAuthenticated => {
                    // Don't sign the user out if they already have working
                    // accounts (e.g. an add-account attempt was cancelled).
                    if self.accounts.is_empty() {
                        self.authenticated = false;
                    }
                    if self.wizard == WizardStep::Connecting {
                        // Return to the most relevant entry step.
                        self.wizard = match &self.discovered {
                            Some(s) if s.auth == crate::autoconfig::AuthKind::Password => {
                                WizardStep::Password
                            }
                            Some(_) => WizardStep::GoogleSetup,
                            None => WizardStep::Manual,
                        };
                    }
                }
                Event::Messages(list) => self.messages = list,
                Event::UnreadCounts(counts) => self.unread_counts = counts,
                Event::SyncStarted(email) => {
                    self.syncing.insert(email);
                    // A new foreground sync re-arms a previously dismissed overlay.
                    self.sync_overlay_dismissed = false;
                }
                Event::SyncFinished(email) => {
                    self.syncing.remove(&email);
                }
                Event::SpamMessages(list) => {
                    self.spam_messages = list;
                    self.spam_loaded = true;
                }
                Event::UpdateAvailable { version, notes } => {
                    self.update_version = Some(version);
                    self.update_notes = notes;
                    self.show_update = true;
                }
                Event::UpdateInstalled => {
                    self.updating = false;
                    self.update_installed = true;
                }
                Event::ClientCredentialsDetected {
                    client_id,
                    client_secret,
                } => {
                    // Auto-captured a downloaded credentials file: fill the
                    // form and go straight to the browser consent flow.
                    if self.wizard == WizardStep::GoogleSetup {
                        self.form_client_id = client_id;
                        self.form_client_secret = client_secret;
                        self.status = "Found your downloaded credentials — signing in…".to_string();
                        self.start_google_auth();
                    }
                }
                Event::MessageBody(body) => {
                    // Only display if it still matches the selected message.
                    if Some(body.uid) == self.selected {
                        self.body = Some(body);
                    }
                }
                Event::MessageSent => {
                    self.sending = false;
                    self.compose_open = false;
                    self.compose_from.clear();
                    self.compose_to.clear();
                    self.compose_subject.clear();
                    self.compose_body.clear();
                }
                Event::CalendarEvents(events) => {
                    self.calendar_events = events;
                    self.calendar_loaded = true;
                }
                Event::CalendarEventCreated(event) => {
                    // Show the new event immediately, then reload to re-sort.
                    self.calendar_events.push(event);
                    self.new_event_open = false;
                    self.ev_summary.clear();
                    self.ev_location.clear();
                    self.send(Command::LoadCalendar);
                }
                Event::Error(e) => {
                    self.error = Some(e);
                    self.sending = false;
                    self.updating = false;
                    if self.wizard == WizardStep::Connecting {
                        self.wizard = match &self.discovered {
                            Some(s) if s.auth == crate::autoconfig::AuthKind::Password => {
                                WizardStep::Password
                            }
                            Some(_) => WizardStep::GoogleSetup,
                            None => WizardStep::Manual,
                        };
                    }
                }
            }
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.cmd_tx.send(cmd);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        self.top_bar(ui);
        self.update_window(ui.ctx());

        if self.authenticated && !self.adding_account {
            self.compose_window(ui.ctx());
            self.new_event_window(ui.ctx());
            match self.view {
                ViewMode::Mail => {
                    self.accounts_panel(ui);
                    self.message_list(ui);
                    self.message_view(ui);
                }
                ViewMode::Spam => {
                    self.accounts_panel(ui);
                    self.spam_list(ui);
                    self.message_view(ui);
                }
                ViewMode::Calendar => self.calendar_view(ui),
            }
        } else {
            self.setup_screen(ui);
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("electronicmail");
                if self.authenticated {
                    ui.separator();
                    if ui.button("✉ Compose").clicked() {
                        self.start_compose();
                    }
                    // Mail / Calendar view switch.
                    if ui
                        .selectable_label(self.view == ViewMode::Mail, "Mail")
                        .clicked()
                    {
                        self.view = ViewMode::Mail;
                    }
                    let spam_label = if self.spam_messages.is_empty() {
                        "Spam".to_string()
                    } else {
                        format!("Spam ({})", self.spam_messages.len())
                    };
                    if ui
                        .selectable_label(self.view == ViewMode::Spam, spam_label)
                        .clicked()
                    {
                        self.view = ViewMode::Spam;
                        self.selected = None;
                        self.body = None;
                        if !self.spam_loaded {
                            self.send(Command::LoadSpam);
                        }
                    }
                    if self.has_calendar
                        && ui
                            .selectable_label(self.view == ViewMode::Calendar, "Calendar")
                            .clicked()
                    {
                        self.view = ViewMode::Calendar;
                        if !self.calendar_loaded {
                            self.send(Command::LoadCalendar);
                        }
                    }
                    ui.separator();

                    if self.view == ViewMode::Mail {
                        let search = ui.add(
                            egui::TextEdit::singleline(&mut self.search_text)
                                .hint_text(
                                    "Search  (try: from:alice subject:invoice -draft after:2024-01-01)",
                                )
                                .desired_width(360.0),
                        );
                        let submitted =
                            search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Search").clicked() || submitted {
                            self.send(Command::Search(self.search_text.clone()));
                        }
                        if ui.button("Clear").clicked() {
                            self.search_text.clear();
                            self.send(Command::Search(String::new()));
                        }
                        if ui.button("⟳ Sync").clicked() {
                            self.send(Command::Sync);
                        }
                        ui.toggle_value(&mut self.show_help, "？");
                    } else if self.view == ViewMode::Spam {
                        if ui.button("⟳ Refresh").clicked() {
                            self.send(Command::LoadSpam);
                        }
                    } else if ui.button("⟳ Refresh").clicked() {
                        self.send(Command::LoadCalendar);
                    }
                }
            });

            // Status / account info on their own row so they never overlap the
            // search bar at narrow window widths.
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&self.status).weak());
                if self.authenticated {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(self.active_email()).weak());
                        if let Some(version) = self.update_version.clone() {
                            let label = if self.update_installed {
                                "⟳ Restart to update".to_string()
                            } else {
                                format!("⟱ Update to {version}")
                            };
                            if ui
                                .button(egui::RichText::new(label).color(egui::Color32::from_rgb(
                                    90, 170, 90,
                                )))
                                .clicked()
                            {
                                self.show_update = true;
                            }
                        }
                    });
                }
            });

            if self.show_help && self.authenticated {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Operators:  \"exact phrase\"  +required  -exclude   \
                         from:  to:  subject:  body:   after:YYYY-MM-DD  before:YYYY-MM-DD   \
                         is:unread  is:read  is:flagged",
                    )
                    .small()
                    .weak(),
                );
            }

            if let Some(err) = &self.error {
                ui.add_space(2.0);
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), format!("⚠ {err}"));
            }
            ui.add_space(4.0);
        });
    }

    fn setup_screen(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.set_max_width(560.0);
                if self.adding_account {
                    ui.horizontal(|ui| {
                        if ui.button("\u{2190} Back to mailboxes").clicked() {
                            self.adding_account = false;
                            self.error = None;
                        }
                    });
                    ui.add_space(8.0);
                }
                match self.wizard {
                    WizardStep::Email => self.wizard_email(ui),
                    WizardStep::Discovering => self.wizard_discovering(ui),
                    WizardStep::Password => self.wizard_password(ui),
                    WizardStep::Manual => self.wizard_manual(ui),
                    WizardStep::GoogleSetup => self.google_setup(ui),
                    WizardStep::Connecting => self.wizard_connecting(ui),
                }

                if let Some(err) = &self.error {
                    ui.add_space(14.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), format!("⚠ {err}"));
                }
            });
        });
    }

    /// Step 1 — the only thing most users ever type: their email address.
    fn wizard_email(&mut self, ui: &mut egui::Ui) {
        ui.heading("Add your email account");
        ui.add_space(6.0);
        ui.label("Enter your email address and we'll set everything up for you.");
        ui.add_space(22.0);

        let entered = ui
            .add_sized(
                [380.0, 30.0],
                egui::TextEdit::singleline(&mut self.form_email)
                    .hint_text("you@example.com")
                    .horizontal_align(egui::Align::Center),
            )
            .lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter));

        ui.add_space(16.0);
        let ready = self.form_email.contains('@')
            && self
                .form_email
                .split('@')
                .nth(1)
                .is_some_and(|d| d.contains('.'));
        let clicked = ui
            .add_enabled(
                ready,
                egui::Button::new("Continue").min_size(egui::vec2(380.0, 32.0)),
            )
            .clicked();

        if (clicked || (entered && ready)) && ready {
            self.begin_discovery();
        }

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(
                "Gmail, Outlook, Yahoo, iCloud and most other providers are detected \
                 automatically — no settings to configure.",
            )
            .small()
            .weak(),
        );
    }

    /// Send the email off for autoconfig and show the lookup spinner.
    fn begin_discovery(&mut self) {
        self.error = None;
        self.discovered = None;
        let email = self.form_email.trim().to_string();
        self.form_email = email.clone();
        self.form_username = email.clone();
        self.wizard = WizardStep::Discovering;
        self.send(Command::Discover(email));
    }

    /// Step 2 — waiting for autoconfig to resolve the provider.
    fn wizard_discovering(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Setting up {}…", self.form_email));
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new(&self.status).small().weak());
        ui.add_space(18.0);
        if ui.button("Cancel").clicked() {
            self.wizard = WizardStep::Email;
        }
    }

    /// Apply discovered settings — auto-start OAuth, or ask for a password.
    fn on_discovered(&mut self, settings: crate::autoconfig::MailSettings) {
        use crate::autoconfig::AuthKind;
        self.error = None;
        self.form_imap_host = settings.imap_host.clone();
        self.form_imap_port = settings.imap_port.to_string();
        self.form_smtp_host = settings.smtp_host.clone();
        self.form_smtp_port = settings.smtp_port.to_string();
        let auth = settings.auth;
        self.discovered = Some(settings);

        match auth {
            AuthKind::OAuthGoogle => {
                // If this build bundles an OAuth client, sign in seamlessly.
                // Otherwise guide the user through bring-your-own-client setup.
                if crate::autoconfig::oauth_clients::google_configured() {
                    self.form_client_id =
                        crate::autoconfig::oauth_clients::GOOGLE_CLIENT_ID.to_string();
                    self.form_client_secret =
                        crate::autoconfig::oauth_clients::GOOGLE_CLIENT_SECRET.to_string();
                    self.start_google_auth();
                } else {
                    self.begin_google_setup();
                }
            }
            AuthKind::Password => {
                self.wizard = WizardStep::Password;
            }
        }
    }

    /// Step 3a — provider known, just collect the password / app-password.
    fn wizard_password(&mut self, ui: &mut egui::Ui) {
        let (provider_name, hint, help_url) = match &self.discovered {
            Some(s) => (
                s.provider_name.clone(),
                s.app_password_hint.clone(),
                s.help_url.clone(),
            ),
            None => (String::new(), None, None),
        };

        ui.horizontal(|ui| {
            if ui.button("← Back").clicked() {
                self.error = None;
                self.wizard = WizardStep::Email;
            }
            ui.add_space(4.0);
            ui.heading(format!("Sign in to {provider_name}"));
        });
        ui.add_space(8.0);
        ui.label(egui::RichText::new(&self.form_email).weak());
        ui.add_space(12.0);

        if let Some(hint) = hint {
            ui.label(egui::RichText::new(hint).weak());
            if let Some(url) = help_url {
                if ui.link("How to create an app-password").clicked() {
                    let _ = webbrowser::open(&url);
                }
            }
            ui.add_space(10.0);
        }

        ui.add(
            egui::TextEdit::singleline(&mut self.form_password)
                .password(true)
                .hint_text("password or app-password")
                .desired_width(380.0),
        );

        ui.add_space(16.0);
        let ready = !self.form_password.is_empty();
        if ui
            .add_enabled(
                ready,
                egui::Button::new("Connect").min_size(egui::vec2(160.0, 30.0)),
            )
            .clicked()
        {
            self.start_password_auth();
        }
    }

    /// Step 3b — provider not recognised; collect server settings by hand.
    fn wizard_manual(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("← Back").clicked() {
                self.error = None;
                self.wizard = WizardStep::Email;
            }
            ui.add_space(4.0);
            ui.heading("Server settings");
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "We couldn't detect your provider. Enter the IMAP/SMTP details from \
                 your email provider's help pages.",
            )
            .weak(),
        );
        ui.add_space(12.0);

        egui::Grid::new("manual_grid")
            .num_columns(2)
            .spacing([12.0, 10.0])
            .show(ui, |ui| {
                ui.label("Username");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form_username)
                        .hint_text("usually your full email address")
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("IMAP server");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form_imap_host)
                        .hint_text("imap.example.com")
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("IMAP port");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form_imap_port)
                        .hint_text("993")
                        .desired_width(80.0),
                );
                ui.end_row();

                ui.label("SMTP server");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form_smtp_host)
                        .hint_text("smtp.example.com")
                        .desired_width(360.0),
                );
                ui.end_row();

                ui.label("SMTP port");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form_smtp_port)
                        .hint_text("465 (TLS) or 587 (STARTTLS)")
                        .desired_width(120.0),
                );
                ui.end_row();

                ui.label("Password");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form_password)
                        .password(true)
                        .hint_text("password or app-password")
                        .desired_width(360.0),
                );
                ui.end_row();
            });

        ui.add_space(16.0);
        let port_ok = self.form_imap_port.trim().parse::<u16>().is_ok();
        let smtp_port_ok = self.form_smtp_port.trim().parse::<u16>().is_ok();
        let ready = !self.form_imap_host.trim().is_empty()
            && port_ok
            && !self.form_smtp_host.trim().is_empty()
            && smtp_port_ok
            && !self.form_password.is_empty();
        if ui
            .add_enabled(
                ready,
                egui::Button::new("Connect").min_size(egui::vec2(160.0, 30.0)),
            )
            .clicked()
        {
            self.start_password_auth();
        }
    }

    /// Build a password `AuthChoice` from the current form and start auth.
    fn start_password_auth(&mut self) {
        let username = if self.form_username.trim().is_empty() {
            self.form_email.trim().to_string()
        } else {
            self.form_username.trim().to_string()
        };
        self.start_auth(AuthChoice::Password {
            imap_host: self.form_imap_host.trim().to_string(),
            imap_port: self.form_imap_port.trim().parse::<u16>().unwrap_or(993),
            smtp_host: self.form_smtp_host.trim().to_string(),
            smtp_port: self.form_smtp_port.trim().parse::<u16>().unwrap_or(465),
            username,
            password: self.form_password.clone(),
        });
    }

    /// Enter the bring-your-own-client Google setup step and begin watching the
    /// Downloads folder for the credentials file the user will download.
    fn begin_google_setup(&mut self) {
        self.error = None;
        if !self.cred_watch_active {
            self.send(Command::WatchClientCredentials);
            self.cred_watch_active = true;
        }
        self.wizard = WizardStep::GoogleSetup;
    }

    /// Leave the Google setup step, stopping the credential watcher.
    fn cancel_google_setup(&mut self) {
        if self.cred_watch_active {
            self.send(Command::StopClientCredentialsWatch);
            self.cred_watch_active = false;
        }
        self.error = None;
        self.wizard = WizardStep::Email;
    }

    /// Start Google OAuth with the credentials currently in the form.
    fn start_google_auth(&mut self) {
        if self.cred_watch_active {
            self.send(Command::StopClientCredentialsWatch);
            self.cred_watch_active = false;
        }
        let client_id = self.form_client_id.trim().to_string();
        let client_secret = self.form_client_secret.trim().to_string();
        self.start_auth(AuthChoice::Google {
            client_id,
            client_secret,
        });
    }

    /// Step 3c — bring-your-own-client Google OAuth setup. Guides the user
    /// through creating a free OAuth client in their own Google Cloud project,
    /// then auto-captures the credentials they download and signs in.
    fn google_setup(&mut self, ui: &mut egui::Ui) {
        const URL_PROJECT: &str = "https://console.cloud.google.com/projectcreate";
        const URL_GMAIL_API: &str =
            "https://console.cloud.google.com/apis/library/gmail.googleapis.com";
        const URL_CALENDAR_API: &str =
            "https://console.cloud.google.com/apis/library/calendar-json.googleapis.com";
        const URL_CONSENT: &str = "https://console.cloud.google.com/auth/branding";
        const URL_AUDIENCE: &str = "https://console.cloud.google.com/auth/audience";
        const URL_CLIENTS: &str =
            "https://console.cloud.google.com/auth/clients/create?type=desktop";

        ui.horizontal(|ui| {
            if ui.button("← Back").clicked() {
                self.cancel_google_setup();
            }
            ui.add_space(4.0);
            ui.heading("Connect Gmail — free, no fees");
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new(&self.form_email).weak());
        ui.add_space(8.0);
        ui.label(
            "A one-time setup links Gmail using your own free Google Cloud OAuth \
             client. Follow the steps — the app finishes automatically the moment \
             you download your credentials file.",
        );
        ui.add_space(12.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong("Step 1 · Create a Google Cloud project");
            if ui.button("Open Google Cloud → New project").clicked() {
                let _ = webbrowser::open(URL_PROJECT);
            }
            ui.add_space(8.0);

            ui.strong("Step 2 · Enable the APIs");
            ui.horizontal(|ui| {
                if ui.button("Enable Gmail API").clicked() {
                    let _ = webbrowser::open(URL_GMAIL_API);
                }
                if ui.button("Enable Calendar API").clicked() {
                    let _ = webbrowser::open(URL_CALENDAR_API);
                }
            });
            ui.add_space(8.0);

            ui.strong("Step 3 · Configure the consent screen");
            ui.label(
                egui::RichText::new("Choose “External”, then add your own email as a Test user.")
                    .small()
                    .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button("Consent screen").clicked() {
                    let _ = webbrowser::open(URL_CONSENT);
                }
                if ui.button("Add test user").clicked() {
                    let _ = webbrowser::open(URL_AUDIENCE);
                }
            });
            ui.add_space(8.0);

            ui.strong("Step 4 · Create the OAuth client");
            ui.label(
                egui::RichText::new("Application type: “Desktop app”, then click Download JSON.")
                    .small()
                    .weak(),
            );
            if ui.button("Create OAuth client (Desktop app)").clicked() {
                let _ = webbrowser::open(URL_CLIENTS);
            }
        });

        ui.add_space(14.0);

        let captured =
            !self.form_client_id.trim().is_empty() && !self.form_client_secret.trim().is_empty();
        ui.horizontal(|ui| {
            if captured {
                ui.colored_label(
                    egui::Color32::from_rgb(80, 170, 90),
                    "✔ Credentials captured",
                );
            } else {
                ui.spinner();
                ui.label("Watching your Downloads folder for the credentials file…");
            }
        });
        ui.add_space(8.0);

        egui::CollapsingHeader::new("Enter credentials manually").show(ui, |ui| {
            ui.label(
                egui::RichText::new("Paste the downloaded JSON, or the client ID and secret:")
                    .small()
                    .weak(),
            );
            let paste = ui.add(
                egui::TextEdit::multiline(&mut self.form_client_paste)
                    .desired_rows(3)
                    .desired_width(440.0)
                    .hint_text("Paste client_secret_*.json contents here"),
            );
            if paste.changed() {
                if let Some((id, secret)) =
                    crate::auth::parse_client_credentials(&self.form_client_paste)
                {
                    self.form_client_id = id;
                    self.form_client_secret = secret;
                }
            }
            egui::Grid::new("byoc_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Client ID");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form_client_id)
                            .desired_width(360.0)
                            .hint_text("…apps.googleusercontent.com"),
                    );
                    ui.end_row();
                    ui.label("Client secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form_client_secret)
                            .password(true)
                            .desired_width(360.0)
                            .hint_text("GOCSPX-…"),
                    );
                    ui.end_row();
                });
        });

        ui.add_space(14.0);
        let ready =
            !self.form_client_id.trim().is_empty() && !self.form_client_secret.trim().is_empty();
        if ui
            .add_enabled(
                ready,
                egui::Button::new("Connect with Google").min_size(egui::vec2(220.0, 32.0)),
            )
            .clicked()
        {
            self.start_google_auth();
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Your credentials stay on this device and use your own free Google \
                 project, so there are no verification fees.",
            )
            .small()
            .weak(),
        );
    }

    /// Kick off authentication and move to the Connecting step.
    fn start_auth(&mut self, choice: AuthChoice) {
        self.error = None;
        self.auth_url = None;
        self.wizard = WizardStep::Connecting;
        self.send(Command::Authenticate {
            email: self.form_email.trim().to_string(),
            choice,
        });
    }

    /// Step 4 — authentication in progress.
    fn wizard_connecting(&mut self, ui: &mut egui::Ui) {
        let uses_oauth = matches!(
            self.discovered.as_ref().map(|s| s.auth),
            Some(crate::autoconfig::AuthKind::OAuthGoogle)
        );

        ui.add_space(10.0);
        ui.heading("Signing in…");
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(&self.status);
        });

        if uses_oauth {
            ui.add_space(14.0);
            ui.label(
                "Complete the Google consent screen in your browser, then return here. \
                 This window will continue automatically.",
            );
            ui.add_space(8.0);
            if let Some(url) = self.auth_url.clone() {
                if ui.button("Reopen browser sign-in page").clicked() {
                    let _ = webbrowser::open(&url);
                }
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("If your browser didn't open, copy this link:")
                        .small()
                        .weak(),
                );
                let mut url_box = url;
                ui.add(
                    egui::TextEdit::multiline(&mut url_box)
                        .desired_rows(2)
                        .desired_width(440.0),
                );
            }
        }

        ui.add_space(16.0);
        if ui.button("Cancel").clicked() {
            self.auth_url = None;
            self.wizard = WizardStep::Email;
        }
    }

    /// Left-most sidebar: the mailbox/account switcher.
    fn accounts_panel(&mut self, ui: &mut egui::Ui) {
        let accounts = self.accounts.clone();
        egui::Panel::left("accounts")
            .resizable(true)
            .default_size(240.0)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                ui.strong("Mailboxes");
                ui.separator();

                let all_selected = self.scope.is_none();
                let total_unread: i64 = self.unread_counts.values().sum();
                if mailbox_row(
                    ui,
                    0,
                    all_selected,
                    "\u{1F4E5}  All inboxes",
                    "All inboxes",
                    total_unread,
                ) && !all_selected
                {
                    self.scope = None;
                    self.selected = None;
                    self.body = None;
                    self.send(Command::SelectScope(None));
                }

                for (i, acc) in accounts.iter().enumerate() {
                    let selected = self.scope.as_deref() == Some(acc.email.as_str());
                    let count = self.unread_counts.get(&acc.email).copied().unwrap_or(0);
                    if mailbox_row(
                        ui,
                        i + 1,
                        selected,
                        acc.email.as_str(),
                        acc.email.as_str(),
                        count,
                    ) && !selected
                    {
                        self.scope = Some(acc.email.clone());
                        self.selected = None;
                        self.body = None;
                        self.send(Command::SelectScope(Some(acc.email.clone())));
                    }
                }

                ui.add_space(6.0);
                ui.separator();
                if ui.button("\u{FF0B} Add account").clicked() {
                    self.start_add_account();
                }
            });
    }

    /// Open the setup wizard to add another account without signing out.
    fn start_add_account(&mut self) {
        self.adding_account = true;
        self.error = None;
        self.discovered = None;
        self.wizard = WizardStep::Email;
        self.form_email.clear();
        self.form_password.clear();
        self.form_username.clear();
    }

    /// The account whose identity outgoing mail uses: the active scope, or the
    /// first configured account when viewing all inboxes.
    fn active_email(&self) -> &str {
        match &self.scope {
            Some(e) => e,
            None => &self.account_email,
        }
    }

    /// Open a message: mark it read, and — when in unread-only mode — drop the
    /// *previously* read message from the list now that the user has moved on
    /// (so it doesn't linger but also doesn't vanish while still being read).
    fn select_message(&mut self, uid: i64) {
        if self.unread_only {
            if let Some(prev) = self.selected {
                if prev != uid {
                    self.messages
                        .retain(|m| m.uid == uid || !(m.uid == prev && m.seen));
                    self.spam_messages
                        .retain(|m| m.uid == uid || !(m.uid == prev && m.seen));
                }
            }
        }
        self.selected = Some(uid);
        self.body = None;
        self.mark_seen_local(uid);
        self.send(Command::OpenMessage(uid));
    }

    /// Optimistically flag a just-opened message as read in the loaded lists so
    /// its row un-bolds immediately. It stays visible until the user moves to
    /// another message (see `select_message`) or the next refresh.
    fn mark_seen_local(&mut self, uid: i64) {
        for msg in self
            .messages
            .iter_mut()
            .chain(self.spam_messages.iter_mut())
        {
            if msg.uid == uid {
                msg.seen = true;
            }
        }
    }

    fn message_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("list")
            .resizable(true)
            .default_size(380.0)
            .show_inside(ui, |ui| {
                let panel_rect = ui.max_rect();
                ui.add_space(4.0);
                let title = match &self.scope {
                    Some(e) => e.clone(),
                    None => "All inboxes".to_string(),
                };
                ui.horizontal(|ui| {
                    ui.strong(title);
                    ui.label(egui::RichText::new(format!("({})", self.messages.len())).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if self.unread_only {
                            "Show read too"
                        } else {
                            "Unread only"
                        };
                        if ui.button(label).clicked() {
                            self.unread_only = !self.unread_only;
                            self.send(Command::SetUnreadOnly(self.unread_only));
                        }
                    });
                });
                ui.separator();

                let show_account = self.scope.is_none();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let mut to_open = None;
                    for (i, msg) in self.messages.iter().enumerate() {
                        let selected = self.selected == Some(msg.uid);
                        let response =
                            render_list_item(ui, msg, selected, show_account, i % 2 == 1);
                        if response.clicked() {
                            to_open = Some(msg.uid);
                        }
                        ui.separator();
                    }
                    if let Some(uid) = to_open {
                        self.select_message(uid);
                    }
                });

                self.sync_overlay(ui, panel_rect);
            });
    }

    fn spam_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("spam_list")
            .resizable(true)
            .default_size(380.0)
            .show_inside(ui, |ui| {
                let panel_rect = ui.max_rect();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("Spam");
                    ui.label(egui::RichText::new(format!("({})", self.spam_messages.len())).weak());
                });
                ui.label(
                    egui::RichText::new(
                        "Filtered messages. Open one to restore it or allow the sender.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();

                if self.spam_messages.is_empty() {
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("No spam — nice and tidy.").weak());
                    self.sync_overlay(ui, panel_rect);
                    return;
                }

                let show_account = self.scope.is_none();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let mut to_open = None;
                    for (i, msg) in self.spam_messages.iter().enumerate() {
                        let selected = self.selected == Some(msg.uid);
                        let response =
                            render_list_item(ui, msg, selected, show_account, i % 2 == 1);
                        if response.clicked() {
                            to_open = Some(msg.uid);
                        }
                        ui.separator();
                    }
                    if let Some(uid) = to_open {
                        self.select_message(uid);
                    }
                });

                self.sync_overlay(ui, panel_rect);
            });
    }

    /// Whether the currently viewed mailbox scope is mid-sync and the overlay
    /// hasn't been dismissed. For "All inboxes" (`scope == None`) this is true
    /// while *any* account is still updating.
    fn is_scope_syncing(&self) -> bool {
        if self.sync_overlay_dismissed || self.syncing.is_empty() {
            return false;
        }
        match &self.scope {
            Some(email) => self.syncing.contains(email),
            None => true,
        }
    }

    /// Draw the "Updating…" spinner overlay across `rect` when the current
    /// mailbox is syncing. Includes a ✕ to dismiss it manually.
    fn sync_overlay(&mut self, ui: &egui::Ui, rect: egui::Rect) {
        if !self.is_scope_syncing() {
            return;
        }
        // Animate the spinner even while the worker is busy on its own thread.
        ui.ctx().request_repaint();

        // Dim the panel behind the card.
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("sync_overlay_dim"),
        ));
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(110));

        let mut dismiss = false;
        egui::Area::new(egui::Id::new("sync_overlay_card"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.center() - egui::vec2(110.0, 24.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(200.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.add_space(4.0);
                        ui.strong("Updating…");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("✕").on_hover_text("Dismiss").clicked() {
                                dismiss = true;
                            }
                        });
                    });
                });
            });
        if dismiss {
            self.sync_overlay_dismissed = true;
        }
    }

    fn message_view(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let Some(uid) = self.selected else {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Select a message").weak());
                });
                return;
            };
            let Some(msg) = self
                .messages
                .iter()
                .chain(self.spam_messages.iter())
                .find(|m| m.uid == uid)
                .cloned()
            else {
                return;
            };

            ui.add_space(6.0);
            ui.heading(if msg.subject.is_empty() {
                "(no subject)"
            } else {
                &msg.subject
            });
            ui.add_space(6.0);

            egui::Grid::new("headers")
                .num_columns(2)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("From").weak());
                    // Always show the full address to help spot spoofing (goal #3).
                    ui.horizontal(|ui| {
                        if !msg.from_name.is_empty() {
                            ui.label(&msg.from_name);
                        }
                        ui.label(egui::RichText::new(format!("<{}>", msg.from_addr)).strong());
                    });
                    ui.end_row();

                    if !msg.to_addrs.is_empty() {
                        ui.label(egui::RichText::new("To").weak());
                        ui.label(&msg.to_addrs);
                        ui.end_row();
                    }

                    ui.label(egui::RichText::new("Date").weak());
                    ui.label(format_date(msg.date_ts));
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("↩ Reply").clicked() {
                    self.start_reply(&msg);
                }
            });
            ui.add_space(2.0);
            self.spam_controls(ui, &msg);

            ui.separator();
            ui.add_space(6.0);

            egui::ScrollArea::vertical().show(ui, |ui| {
                match &self.body {
                    // Prefer rich HTML when present — sanitized so no remote
                    // content (images, scripts, trackers) is ever fetched —
                    // falling back to plain text.
                    Some(EmailBody {
                        html: Some(html), ..
                    }) if !html.trim().is_empty() => {
                        crate::htmlview::render(ui, html);
                    }
                    Some(EmailBody {
                        text: Some(text), ..
                    }) if !text.trim().is_empty() => {
                        ui.add(egui::Label::new(text).wrap());
                    }
                    Some(_) => {
                        ui.label(egui::RichText::new("(empty message)").weak());
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Loading…");
                        });
                    }
                }
            });
        });
    }

    /// Spam verdict banner + the actions available for the open message.
    fn spam_controls(&mut self, ui: &mut egui::Ui, msg: &EmailSummary) {
        if msg.is_spam {
            ui.colored_label(egui::Color32::from_rgb(220, 90, 80), "⚠ Filtered as spam");
            if !msg.spam_reasons.is_empty() {
                ui.label(egui::RichText::new(&msg.spam_reasons).small().weak());
            }
        } else if msg.spam_score >= 0.3 {
            ui.colored_label(
                egui::Color32::from_rgb(210, 150, 70),
                format!("Caution: possible spam ({:.0}%)", msg.spam_score * 100.0),
            );
            if !msg.spam_reasons.is_empty() {
                ui.label(egui::RichText::new(&msg.spam_reasons).small().weak());
            }
        }

        ui.horizontal(|ui| {
            if msg.is_spam {
                if ui.button("✓ Not spam").clicked() {
                    self.send(Command::MarkNotSpam(msg.uid));
                }
                if ui.button("Allow sender").clicked() {
                    self.send(Command::AllowSender(msg.uid));
                }
            } else {
                if ui.button("🚫 Mark as spam").clicked() {
                    self.send(Command::MarkSpam(msg.uid));
                }
                if ui.button("Block sender").clicked() {
                    self.send(Command::BlockSender(msg.uid));
                }
            }
        });
    }

    // --- Compose ---

    fn update_window(&mut self, ctx: &egui::Context) {
        if !self.show_update {
            return;
        }
        let version = self.update_version.clone().unwrap_or_default();
        let mut open = true;
        egui::Window::new("Software update")
            .collapsible(false)
            .resizable(true)
            .default_size([460.0, 320.0])
            .open(&mut open)
            .show(ctx, |ui| {
                if self.update_installed {
                    ui.label(
                        egui::RichText::new(format!("electronicmail {version} is installed."))
                            .strong(),
                    );
                    ui.label("Restart the app to start using the new version.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Restart now").clicked() {
                            restart_app();
                        }
                        if ui.button("Later").clicked() {
                            self.show_update = false;
                        }
                    });
                    return;
                }

                ui.label(
                    egui::RichText::new(format!("electronicmail {version} is available."))
                        .strong()
                        .size(16.0),
                );
                ui.label(
                    egui::RichText::new(format!("You're running {}.", env!("CARGO_PKG_VERSION")))
                        .weak(),
                );

                if !self.update_notes.trim().is_empty() {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Release notes").weak());
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .show(ui, |ui| {
                            ui.label(&self.update_notes);
                        });
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if self.updating {
                        ui.spinner();
                        ui.label("Downloading and installing…");
                    } else {
                        if ui.button("Update now").clicked() {
                            self.updating = true;
                            self.error = None;
                            self.send(Command::InstallUpdate);
                        }
                        if ui.button("Later").clicked() {
                            self.show_update = false;
                        }
                    }
                });
            });
        // A window closed via its [x] dismisses the prompt (unless mid-install).
        if !open && !self.updating {
            self.show_update = false;
        }
    }

    /// Open the composer for a brand-new message from the active mailbox.
    ///
    /// Reuses the same window as a reply, but starts from a clean slate so no
    /// leftover recipient, subject or quoted body carries over from a prior
    /// reply draft.
    fn start_compose(&mut self) {
        self.compose_from = self.active_email().to_string();
        self.compose_to.clear();
        self.compose_subject.clear();
        self.compose_body.clear();
        self.error = None;
        self.compose_open = true;
    }

    /// Pre-fill the composer with a reply to `msg`.
    ///
    /// The From address defaults to the account that *received* the message, so
    /// the reply goes out from the same mailbox — but it stays editable so the
    /// user can send from a different account.
    fn start_reply(&mut self, msg: &EmailSummary) {
        self.compose_from = if msg.account.is_empty() {
            self.active_email().to_string()
        } else {
            msg.account.clone()
        };
        self.compose_to = msg.from_addr.clone();
        self.compose_subject = reply_subject(&msg.subject);
        self.compose_body = quote_reply(msg, self.body.as_ref());
        self.error = None;
        self.compose_open = true;
    }

    fn compose_window(&mut self, ctx: &egui::Context) {
        let mut open = self.compose_open;
        egui::Window::new("New message")
            .open(&mut open)
            .resizable(true)
            .default_size([540.0, 460.0])
            .show(ctx, |ui| {
                // Default the From address to the active mailbox.
                if self.compose_from.is_empty() {
                    self.compose_from = self.active_email().to_string();
                }
                let from_options: Vec<String> = if self.accounts.is_empty() {
                    vec![self.active_email().to_string()]
                } else {
                    self.accounts.iter().map(|a| a.email.clone()).collect()
                };

                egui::Grid::new("compose_grid")
                    .num_columns(2)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("From");
                        if from_options.len() > 1 {
                            egui::ComboBox::from_id_salt("compose_from")
                                .width(360.0)
                                .selected_text(self.compose_from.clone())
                                .show_ui(ui, |ui| {
                                    for email in &from_options {
                                        ui.selectable_value(
                                            &mut self.compose_from,
                                            email.clone(),
                                            email.as_str(),
                                        );
                                    }
                                });
                        } else {
                            ui.label(egui::RichText::new(&self.compose_from).weak());
                        }
                        ui.end_row();

                        ui.label("To");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.compose_to)
                                .hint_text("recipient@example.com, another@example.com")
                                .desired_width(400.0),
                        );
                        ui.end_row();

                        ui.label("Subject");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.compose_subject)
                                .desired_width(400.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(6.0);
                // Formatting toolbar — each button rewrites the body's current
                // selection (or inserts markers at the caret).
                let body_id = egui::Id::new("compose_body_editor");
                let sel = load_selection(ui.ctx(), body_id, self.compose_body.chars().count());
                let mut refocus = false;
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("B").strong())
                        .on_hover_text("Bold")
                        .clicked()
                    {
                        let new = wrap_selection(&mut self.compose_body, sel, "**");
                        store_selection(ui.ctx(), body_id, new);
                        refocus = true;
                    }
                    if ui
                        .button(egui::RichText::new("I").italics())
                        .on_hover_text("Italic")
                        .clicked()
                    {
                        let new = wrap_selection(&mut self.compose_body, sel, "*");
                        store_selection(ui.ctx(), body_id, new);
                        refocus = true;
                    }
                    if ui.button("• List").on_hover_text("Bulleted list").clicked() {
                        let new = prefix_lines(&mut self.compose_body, sel, "- ");
                        store_selection(ui.ctx(), body_id, new);
                        refocus = true;
                    }
                    if ui.button("“ Quote").on_hover_text("Block quote").clicked() {
                        let new = prefix_lines(&mut self.compose_body, sel, "> ");
                        store_selection(ui.ctx(), body_id, new);
                        refocus = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("Markdown: **bold**  *italic*")
                                .weak()
                                .small(),
                        );
                    });
                });

                ui.add_space(4.0);
                let output = egui::TextEdit::multiline(&mut self.compose_body)
                    .id(body_id)
                    .desired_rows(12)
                    .desired_width(f32::INFINITY)
                    .hint_text("Write your message…")
                    .show(ui);
                if refocus {
                    output.response.request_focus();
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !self.compose_to.trim().is_empty() && !self.sending;
                    if ui.add_enabled(ready, egui::Button::new("Send")).clicked() {
                        self.sending = true;
                        self.error = None;
                        self.send(Command::SendMessage(worker::Compose {
                            from: self.compose_from.clone(),
                            to: self.compose_to.trim().to_string(),
                            subject: self.compose_subject.clone(),
                            body: self.compose_body.clone(),
                        }));
                    }
                    if self.sending {
                        ui.spinner();
                        ui.label("Sending…");
                    }
                });
            });
        // A window closed via its [x] should reset the open flag.
        if !open {
            self.compose_open = false;
        }
    }

    // --- Calendar ---

    fn calendar_view(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Upcoming events");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("＋ New event").clicked() {
                        if self.ev_date.is_empty() {
                            self.ev_date = chrono::Local::now().format("%Y-%m-%d").to_string();
                        }
                        self.new_event_open = true;
                    }
                });
            });
            ui.separator();

            if !self.calendar_loaded {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading your calendar…");
                });
                return;
            }
            if self.calendar_events.is_empty() {
                ui.add_space(12.0);
                ui.label(egui::RichText::new("No upcoming events.").weak());
                return;
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for ev in &self.calendar_events {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("●").color(egui::Color32::from_rgb(90, 150, 240)),
                        );
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&ev.summary).strong());
                            ui.label(egui::RichText::new(format_event_when(ev)).small().weak());
                            if !ev.location.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("📍 {}", ev.location))
                                        .small()
                                        .weak(),
                                );
                            }
                        });
                    });
                    ui.separator();
                }
            });
        });
    }

    fn new_event_window(&mut self, ctx: &egui::Context) {
        let mut open = self.new_event_open;
        egui::Window::new("New event")
            .open(&mut open)
            .resizable(false)
            .default_size([420.0, 320.0])
            .show(ctx, |ui| {
                egui::Grid::new("new_event_grid")
                    .num_columns(2)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Title");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ev_summary).desired_width(300.0),
                        );
                        ui.end_row();

                        ui.label("Date");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ev_date)
                                .hint_text("YYYY-MM-DD")
                                .desired_width(140.0),
                        );
                        ui.end_row();

                        ui.label("Start time");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ev_start_time)
                                .hint_text("HH:MM")
                                .desired_width(100.0),
                        );
                        ui.end_row();

                        ui.label("Duration (min)");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ev_duration_min)
                                .desired_width(80.0),
                        );
                        ui.end_row();

                        ui.label("Location");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ev_location).desired_width(300.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(8.0);
                let parsed = self.parse_new_event();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(parsed.is_some(), egui::Button::new("Create"))
                        .clicked()
                    {
                        if let Some((start, end)) = parsed {
                            self.error = None;
                            self.send(Command::CreateEvent(worker::NewEvent {
                                summary: self.ev_summary.trim().to_string(),
                                location: self.ev_location.trim().to_string(),
                                description: String::new(),
                                start_rfc3339: start,
                                end_rfc3339: end,
                            }));
                        }
                    }
                    if self.parse_new_event().is_none() && !self.ev_summary.trim().is_empty() {
                        ui.label(
                            egui::RichText::new("Check date/time format")
                                .small()
                                .color(egui::Color32::from_rgb(220, 150, 80)),
                        );
                    }
                });
            });
        if !open {
            self.new_event_open = false;
        }
    }

    /// Validate the new-event form and produce RFC3339 start/end timestamps in
    /// the local timezone. Returns `None` until all fields are valid.
    fn parse_new_event(&self) -> Option<(String, String)> {
        use chrono::{NaiveDate, NaiveTime, TimeZone};
        if self.ev_summary.trim().is_empty() {
            return None;
        }
        let date = NaiveDate::parse_from_str(self.ev_date.trim(), "%Y-%m-%d").ok()?;
        let time = NaiveTime::parse_from_str(self.ev_start_time.trim(), "%H:%M").ok()?;
        let duration: i64 = self.ev_duration_min.trim().parse().ok()?;
        if duration <= 0 {
            return None;
        }
        let start_naive = date.and_time(time);
        let start_local = chrono::Local.from_local_datetime(&start_naive).single()?;
        let end_local = start_local + chrono::Duration::minutes(duration);
        Some((start_local.to_rfc3339(), end_local.to_rfc3339()))
    }
}

/// Bump the default text sizes up a touch and brighten body/widget text so the
/// UI reads more comfortably than egui's compact defaults.
fn apply_style(ctx: &egui::Context) {
    use egui::{FontFamily, FontId, TextStyle};

    let mut style = (*ctx.global_style()).clone();
    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(21.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.5, FontFamily::Monospace),
        ),
    ]
    .into_iter()
    .collect();

    // Brighten text across widget states (egui's dark defaults are fairly dim).
    let w = &mut style.visuals.widgets;
    w.noninteractive.fg_stroke.color = egui::Color32::from_gray(215);
    w.inactive.fg_stroke.color = egui::Color32::from_gray(225);
    w.hovered.fg_stroke.color = egui::Color32::from_gray(245);
    w.active.fg_stroke.color = egui::Color32::WHITE;

    ctx.set_global_style(style);
}

/// Background tint for alternating ("zebra") rows. Brighter than egui's
/// `faint_bg_color` for clearer A/B contrast in the lists.
fn zebra_fill(ui: &egui::Ui) -> egui::Color32 {
    let dark = ui.visuals().dark_mode;
    if dark {
        egui::Color32::from_white_alpha(20)
    } else {
        egui::Color32::from_black_alpha(16)
    }
}

/// Relaunch the (freshly updated) executable and exit the current process.
///
/// Inside an AppImage `current_exe()` points at the read-only mount, so we
/// prefer the `$APPIMAGE` path that was just replaced.
fn restart_app() {
    let target = std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok());
    if let Some(path) = target {
        let _ = std::process::Command::new(path).spawn();
    }
    std::process::exit(0);
}

/// Render one mailbox row in the left panel: a zebra-striped background, the
/// label on the left, and an unread badge on the right. Returns `true` if the
/// row was clicked.
fn mailbox_row(
    ui: &mut egui::Ui,
    idx: usize,
    selected: bool,
    label: &str,
    hover: &str,
    count: i64,
) -> bool {
    let fill = if idx % 2 == 1 {
        zebra_fill(ui)
    } else {
        egui::Color32::TRANSPARENT
    };
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // `horizontal` constrains the row to a single line's height; without
            // it a bare right-to-left layout expands to the whole panel height.
            ui.horizontal(|ui| {
                let clicked = ui
                    .selectable_label(selected, egui::RichText::new(label).size(16.0))
                    .on_hover_text(hover)
                    .clicked();
                if count > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        unread_badge(ui, count);
                    });
                }
                clicked
            })
            .inner
        })
        .inner
}

/// Draw a small rounded count "bubble" (e.g. an unread badge) at the cursor.
fn unread_badge(ui: &mut egui::Ui, count: i64) {
    let label = if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    };
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(50, 115, 220))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(label)
                    .color(egui::Color32::WHITE)
                    .small()
                    .strong(),
            );
        });
}

/// Build a "Re: …" subject, avoiding a doubled prefix on an existing reply.
fn reply_subject(subject: &str) -> String {
    let s = subject.trim();
    let already = s
        .get(..3)
        .map(|p| p.eq_ignore_ascii_case("re:"))
        .unwrap_or(false);
    if already {
        s.to_string()
    } else if s.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {s}")
    }
}

/// The quoted body for a reply: blank space to type in, an attribution line,
/// then the original message text with each line prefixed by "> ".
fn quote_reply(msg: &EmailSummary, body: Option<&EmailBody>) -> String {
    let attribution = format!(
        "On {}, {} wrote:",
        format_date(msg.date_ts),
        msg.from_display()
    );
    let original = body.and_then(|b| b.text.clone()).unwrap_or_default();
    if original.trim().is_empty() {
        return format!("\n\n{attribution}\n");
    }
    let quoted = original
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\n{attribution}\n{quoted}\n")
}

/// The body editor's current selection as a sorted char range, or the caret at
/// `len` when the editor hasn't been focused yet.
fn load_selection(ctx: &egui::Context, id: egui::Id, len: usize) -> (usize, usize) {
    egui::TextEdit::load_state(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| {
            let r = r.as_sorted_char_range();
            (r.start, r.end)
        })
        .unwrap_or((len, len))
}

/// Persist a selection (char range) for the body editor so it stays visible
/// after a toolbar edit reshapes the text.
fn store_selection(ctx: &egui::Context, id: egui::Id, range: (usize, usize)) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(range.0),
            egui::text::CCursor::new(range.1),
        )));
    egui::TextEdit::store_state(ctx, id, state);
}

/// Wrap the selected character range in `marker` (e.g. `**`), or drop an empty
/// pair at the caret. Returns the new selection covering the inner text.
fn wrap_selection(body: &mut String, sel: (usize, usize), marker: &str) -> (usize, usize) {
    let (start, end) = sel;
    let bs = char_to_byte(body, start);
    let be = char_to_byte(body, end);
    let inner = body[bs..be].to_string();
    let replacement = format!("{marker}{inner}{marker}");
    body.replace_range(bs..be, &replacement);
    let m = marker.chars().count();
    (start + m, end + m)
}

/// Prefix every line touched by the selection with `prefix` (lists/quotes).
/// Returns the new selection spanning the affected lines.
fn prefix_lines(body: &mut String, sel: (usize, usize), prefix: &str) -> (usize, usize) {
    let chars: Vec<char> = body.chars().collect();
    let (start, end) = sel;
    let mut line_start = start.min(chars.len());
    while line_start > 0 && chars[line_start - 1] != '\n' {
        line_start -= 1;
    }
    let block_end = end.min(chars.len());
    let block: String = chars[line_start..block_end].iter().collect();
    let prefixed = block
        .split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let bs = char_to_byte(body, line_start);
    let be = char_to_byte(body, block_end);
    let new_len = prefixed.chars().count();
    body.replace_range(bs..be, &prefixed);
    (line_start, line_start + new_len)
}

/// Byte offset of the `char_idx`-th character (or the string length when past
/// the end), for slicing a `String` by char position.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn render_list_item(
    ui: &mut egui::Ui,
    msg: &EmailSummary,
    selected: bool,
    show_account: bool,
    alt: bool,
) -> egui::Response {
    // A `Frame` sizes its fill and rect to the row's content, so the highlight
    // and the click target cover only this message — not the whole column.
    // Alternate (odd) rows get a faint background for an A/B "zebra" stripe.
    let fill = if selected {
        ui.visuals().selection.bg_fill.gamma_multiply(0.35)
    } else if alt {
        zebra_fill(ui)
    } else {
        egui::Color32::TRANSPARENT
    };
    let response = egui::Frame::new()
        .fill(fill)
        .corner_radius(2.0)
        .inner_margin(egui::Margin::symmetric(4, 3))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    // Lay out the right-hand metadata first so it always keeps
                    // its space, then let the (bold) From line fill what's left
                    // and truncate with an ellipsis instead of overrunning it.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if msg.flagged {
                            ui.label("★");
                        }
                        ui.label(
                            egui::RichText::new(format_date_short(msg.date_ts))
                                .small()
                                .weak(),
                        );
                        if msg.is_spam {
                            ui.label(
                                egui::RichText::new("spam")
                                    .small()
                                    .color(egui::Color32::from_rgb(220, 90, 80)),
                            );
                        }
                        if show_account {
                            let tag = msg.account.split('@').next().unwrap_or(&msg.account);
                            ui.label(egui::RichText::new(tag).small().weak());
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            // The From line is always bold so the sender stands out.
                            ui.add(
                                egui::Label::new(egui::RichText::new(msg.from_display()).strong())
                                    .truncate(),
                            );
                        });
                    });
                });
                let subject = if msg.subject.is_empty() {
                    "(no subject)".to_string()
                } else {
                    msg.subject.clone()
                };
                let subject = egui::RichText::new(subject);
                ui.add(
                    egui::Label::new(if msg.seen { subject } else { subject.strong() }).truncate(),
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(&msg.snippet).small().weak()).truncate(),
                );
            });
        })
        .response;
    response.interact(egui::Sense::click())
}

fn format_date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn format_date_short(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&Local).format("%b %d").to_string())
        .unwrap_or_default()
}

/// Format a calendar event's start (and end time when present) for display.
fn format_event_when(ev: &crate::calendar::CalEvent) -> String {
    if ev.all_day {
        return format!("{}  ·  all day", ev.start);
    }
    let start = DateTime::parse_from_rfc3339(&ev.start)
        .map(|d| {
            d.with_timezone(&Local)
                .format("%a %b %d, %Y  %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| ev.start.clone());
    let end = DateTime::parse_from_rfc3339(&ev.end)
        .map(|d| d.with_timezone(&Local).format("%H:%M").to_string())
        .ok();
    match end {
        Some(end) => format!("{start} – {end}"),
        None => start,
    }
}
