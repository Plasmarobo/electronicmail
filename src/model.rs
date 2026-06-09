//! Core data types shared across the app, worker, storage and UI.

use serde::{Deserialize, Serialize};

/// A lightweight summary of a message used to render the inbox list.
///
/// We intentionally keep the *full* email address (goal #3) rather than only a
/// display name, so the user can spot spoofed senders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailSummary {
    /// Synthetic primary key (stable, unique across all accounts).
    pub uid: i64,
    /// Which account this message belongs to (its email address).
    pub account: String,
    /// Display name of the sender, if any (e.g. "Jane Doe").
    pub from_name: String,
    /// Full sender address (e.g. "jane@example.com").
    pub from_addr: String,
    /// Comma-joined recipient addresses.
    pub to_addrs: String,
    pub subject: String,
    /// Received date as a Unix timestamp (seconds).
    pub date_ts: i64,
    /// Short preview of the body text.
    pub snippet: String,
    pub seen: bool,
    pub flagged: bool,
    /// Spam probability in `0.0..=1.0` assigned at ingest.
    pub spam_score: f32,
    /// Whether this message is currently filtered as spam.
    pub is_spam: bool,
    /// Human-readable reasons for the spam verdict (joined with "; ").
    pub spam_reasons: String,
}

impl EmailSummary {
    /// "Jane Doe <jane@example.com>" or just the address when no name is set.
    pub fn from_display(&self) -> String {
        if self.from_name.trim().is_empty() {
            self.from_addr.clone()
        } else {
            format!("{} <{}>", self.from_name, self.from_addr)
        }
    }
}

/// The full body of a single message, loaded on demand.
#[derive(Debug, Clone, Default)]
pub struct EmailBody {
    pub uid: i64,
    pub text: Option<String>,
    pub html: Option<String>,
}
