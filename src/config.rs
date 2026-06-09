//! Persistent configuration: account settings + cached OAuth refresh token.
//!
//! Stored as TOML under the platform config directory so secrets stay out of
//! the working tree.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Legacy single-account field, kept only so old config files still load.
    /// Migrated into `accounts` on load and never written back.
    #[serde(default, skip_serializing)]
    pub account: Option<AccountConfig>,
    /// All configured mail accounts.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub spam: SpamConfig,
}

impl AppConfig {
    /// Fold a legacy single `account` into `accounts` (one-time migration).
    pub fn migrate(&mut self) {
        if let Some(acc) = self.account.take() {
            if !self.accounts.iter().any(|a| a.email == acc.email) {
                self.accounts.push(acc);
            }
        }
    }

    /// Find an account by its email address.
    pub fn account(&self, email: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.email == email)
    }

    /// Mutable lookup of an account by email.
    pub fn account_mut(&mut self, email: &str) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| a.email == email)
    }

    /// Insert a new account or replace an existing one with the same email.
    pub fn upsert_account(&mut self, account: AccountConfig) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.email == account.email) {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }
}

/// Spam-filtering preferences and user-managed sender lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpamConfig {
    /// Addresses or bare domains that are always treated as spam.
    #[serde(default)]
    pub block_list: Vec<String>,
    /// Addresses or bare domains that are never treated as spam.
    #[serde(default)]
    pub allow_list: Vec<String>,
    /// Score (0.0..=1.0) at or above which a message is filtered as spam.
    #[serde(default = "default_spam_threshold")]
    pub threshold: f32,
}

impl Default for SpamConfig {
    fn default() -> Self {
        Self {
            block_list: Vec::new(),
            allow_list: Vec::new(),
            threshold: default_spam_threshold(),
        }
    }
}

fn default_spam_threshold() -> f32 {
    0.5
}

/// How an account authenticates with its mail server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AuthMethod {
    /// Google OAuth2 (Gmail / Workspace) via the browser loopback flow.
    #[default]
    OAuthGoogle,
    /// Plain IMAP LOGIN with a password or provider app-password.
    Password,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    /// SMTP server for sending mail.
    #[serde(default)]
    pub smtp_host: String,
    /// SMTP port. 465 = implicit TLS; anything else = STARTTLS.
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Which authentication flow this account uses.
    #[serde(default)]
    pub auth_method: AuthMethod,

    // --- OAuth (Google) ---
    /// Google Cloud OAuth2 "Desktop app" client id.
    #[serde(default)]
    pub client_id: String,
    /// Matching client secret. (Desktop clients are not truly confidential, but
    /// Google still issues a secret that must be sent during token exchange.)
    #[serde(default)]
    pub client_secret: String,
    /// Long-lived refresh token cached after the first successful sign-in.
    pub refresh_token: Option<String>,

    // --- Password / app-password ---
    /// IMAP login username (usually the full email address).
    #[serde(default)]
    pub username: String,
    /// Stored password / app-password for password-based accounts.
    #[serde(default)]
    pub password: Option<String>,
}

fn default_smtp_port() -> u16 {
    465
}

impl AccountConfig {
    /// Build a Google OAuth2 account using the **bundled** client credentials,
    /// filling in Gmail IMAP/SMTP defaults. The user supplies only their email.
    pub fn google(email: String) -> Self {
        use crate::autoconfig::oauth_clients;
        Self {
            email: email.clone(),
            imap_host: "imap.gmail.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".to_string(),
            smtp_port: 465,
            auth_method: AuthMethod::OAuthGoogle,
            client_id: oauth_clients::GOOGLE_CLIENT_ID.to_string(),
            client_secret: oauth_clients::GOOGLE_CLIENT_SECRET.to_string(),
            refresh_token: None,
            username: email,
            password: None,
        }
    }

    /// Build a password / app-password IMAP account.
    pub fn password(
        email: String,
        imap_host: String,
        imap_port: u16,
        smtp_host: String,
        smtp_port: u16,
        username: String,
        password: String,
    ) -> Self {
        Self {
            email,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            auth_method: AuthMethod::Password,
            client_id: String::new(),
            client_secret: String::new(),
            refresh_token: None,
            username,
            password: Some(password),
        }
    }
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("com", "electronicmail", "electronicmail")
        .context("could not determine a config directory for this platform")
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Path to the SQLite mail store.
pub fn database_path() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs.data_dir().join("mail.db"))
}

pub fn load() -> Result<AppConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    let mut cfg: AppConfig = toml::from_str(&text).context("parsing config.toml")?;
    cfg.migrate();
    Ok(cfg)
}

pub fn save(cfg: &AppConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
