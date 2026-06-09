//! egui front-end: account setup, inbox list, search bar and message view.

use std::sync::mpsc::{Receiver, Sender};

use chrono::{DateTime, Local};

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
    /// Active mailbox scope: `None` = combined "All inboxes" view.
    scope: Option<String>,
    /// Hide already-read messages (the default view).
    unread_only: bool,
    /// True while the setup wizard is being used to add an extra account.
    adding_account: bool,

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
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::fonts::install(&cc.egui_ctx);
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
            scope: None,
            unread_only: true,
            adding_account: false,
            wizard: WizardStep::Email,
            discovered: None,
            form_email: String::new(),
            form_imap_host: String::new(),
            form_imap_port: "993".to_string(),
            form_smtp_host: String::new(),
            form_smtp_port: "465".to_string(),
            form_username: String::new(),
            form_password: String::new(),
            auth_url: None,
            show_help: false,
            search_text: String::new(),
            messages: Vec::new(),
            spam_messages: Vec::new(),
            spam_loaded: false,
            selected: None,
            body: None,
            compose_open: false,
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
                            Some(_) => WizardStep::Email,
                            None => WizardStep::Manual,
                        };
                    }
                }
                Event::Messages(list) => self.messages = list,
                Event::SpamMessages(list) => {
                    self.spam_messages = list;
                    self.spam_loaded = true;
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
                    if self.wizard == WizardStep::Connecting {
                        self.wizard = match &self.discovered {
                            Some(s) if s.auth == crate::autoconfig::AuthKind::Password => {
                                WizardStep::Password
                            }
                            Some(_) => WizardStep::Email,
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
                        self.compose_open = true;
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
                // No password needed — go straight to the browser consent flow.
                self.start_auth(AuthChoice::Google);
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
            .default_size(190.0)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                ui.strong("Mailboxes");
                ui.separator();

                let all_selected = self.scope.is_none();
                if ui
                    .selectable_label(all_selected, "\u{1F4E5}  All inboxes")
                    .clicked()
                    && !all_selected
                {
                    self.scope = None;
                    self.selected = None;
                    self.body = None;
                    self.send(Command::SelectScope(None));
                }

                ui.add_space(2.0);
                for acc in &accounts {
                    let selected = self.scope.as_deref() == Some(acc.email.as_str());
                    if ui
                        .selectable_label(selected, acc.email.as_str())
                        .on_hover_text(acc.email.as_str())
                        .clicked()
                        && !selected
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
                    for msg in &self.messages {
                        let selected = self.selected == Some(msg.uid);
                        let response = render_list_item(ui, msg, selected, show_account);
                        if response.clicked() {
                            to_open = Some(msg.uid);
                        }
                        ui.separator();
                    }
                    if let Some(uid) = to_open {
                        self.select_message(uid);
                    }
                });
            });
    }

    fn spam_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("spam_list")
            .resizable(true)
            .default_size(380.0)
            .show_inside(ui, |ui| {
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
                    return;
                }

                let show_account = self.scope.is_none();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let mut to_open = None;
                    for msg in &self.spam_messages {
                        let selected = self.selected == Some(msg.uid);
                        let response = render_list_item(ui, msg, selected, show_account);
                        if response.clicked() {
                            to_open = Some(msg.uid);
                        }
                        ui.separator();
                    }
                    if let Some(uid) = to_open {
                        self.select_message(uid);
                    }
                });
            });
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

    fn compose_window(&mut self, ctx: &egui::Context) {
        let mut open = self.compose_open;
        egui::Window::new("New message")
            .open(&mut open)
            .resizable(true)
            .default_size([520.0, 420.0])
            .show(ctx, |ui| {
                egui::Grid::new("compose_grid")
                    .num_columns(2)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("From");
                        ui.label(egui::RichText::new(self.active_email()).weak());
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
                ui.add(
                    egui::TextEdit::multiline(&mut self.compose_body)
                        .desired_rows(12)
                        .desired_width(f32::INFINITY)
                        .hint_text("Write your message…"),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let ready = !self.compose_to.trim().is_empty() && !self.sending;
                    if ui.add_enabled(ready, egui::Button::new("Send")).clicked() {
                        self.sending = true;
                        self.error = None;
                        self.send(Command::SendMessage(worker::Compose {
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

fn render_list_item(
    ui: &mut egui::Ui,
    msg: &EmailSummary,
    selected: bool,
    show_account: bool,
) -> egui::Response {
    // A `Frame` sizes its fill and rect to the row's content, so the highlight
    // and the click target cover only this message — not the whole column.
    let fill = if selected {
        ui.visuals().selection.bg_fill.gamma_multiply(0.35)
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
                    // The From line is always bold so the sender stands out.
                    ui.label(egui::RichText::new(msg.from_display()).strong());
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
                    });
                });
                let subject = if msg.subject.is_empty() {
                    "(no subject)".to_string()
                } else {
                    msg.subject.clone()
                };
                let subject = egui::RichText::new(subject);
                ui.label(if msg.seen { subject } else { subject.strong() });
                ui.label(egui::RichText::new(&msg.snippet).small().weak());
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
