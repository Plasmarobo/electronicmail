//! IMAP access over TLS using Gmail's XOAUTH2 SASL mechanism.

use anyhow::{Context, Result};
use mail_parser::{Address, MessageParser};
use native_tls::TlsConnector;
use std::net::TcpStream;

use crate::storage::StoredMessage;

pub type Session = imap::Session<native_tls::TlsStream<TcpStream>>;

/// SASL XOAUTH2 authenticator (the format Gmail expects).
struct XOAuth2 {
    user: String,
    access_token: String,
}

impl imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> Self::Response {
        format!(
            "user={}\u{1}auth=Bearer {}\u{1}\u{1}",
            self.user, self.access_token
        )
    }
}

/// Connect to the IMAP server and authenticate with an OAuth2 access token.
pub fn connect(host: &str, port: u16, email: &str, access_token: &str) -> Result<Session> {
    let tls = TlsConnector::builder()
        .build()
        .context("building TLS connector")?;
    let client = imap::connect((host, port), host, &tls)
        .with_context(|| format!("connecting to {host}:{port}"))?;

    let auth = XOAuth2 {
        user: email.to_string(),
        access_token: access_token.to_string(),
    };
    let session = client
        .authenticate("XOAUTH2", &auth)
        .map_err(|(e, _client)| e)
        .context("IMAP XOAUTH2 authentication failed")?;
    Ok(session)
}

/// Connect to the IMAP server and authenticate with a username + password
/// (or provider app-password) using plain IMAP LOGIN over TLS.
pub fn connect_password(host: &str, port: u16, username: &str, password: &str) -> Result<Session> {
    let tls = TlsConnector::builder()
        .build()
        .context("building TLS connector")?;
    let client = imap::connect((host, port), host, &tls)
        .with_context(|| format!("connecting to {host}:{port}"))?;

    let session = client
        .login(username, password)
        .map_err(|(e, _client)| e)
        .context("IMAP login failed — check the username/password (or app-password)")?;
    Ok(session)
}

/// Fetch messages from INBOX.
///
/// * On the first sync (`since_uid == 0`) we fetch only the most recent
///   `initial_limit` messages to stay fast and compact.
/// * On later syncs we fetch UIDs greater than `since_uid`.
pub fn fetch_inbox(
    session: &mut Session,
    account: &str,
    since_uid: i64,
    initial_limit: u32,
) -> Result<Vec<StoredMessage>> {
    let mailbox = session.select("INBOX").context("selecting INBOX")?;
    let total = mailbox.exists;
    if total == 0 {
        return Ok(Vec::new());
    }

    // BODY.PEEK[] retrieves the full RFC822 message without setting the \Seen flag.
    const ITEMS: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[])";

    let parser = MessageParser::default();
    let mut out = Vec::new();

    if since_uid == 0 {
        let start = total.saturating_sub(initial_limit.saturating_sub(1)).max(1);
        let seq = format!("{start}:{total}");
        let fetches = session
            .fetch(seq, ITEMS)
            .context("fetching recent messages")?;
        for f in fetches.iter() {
            if let Some(m) = to_stored(f, account, &parser) {
                out.push(m);
            }
        }
    } else {
        let seq = format!("{}:*", since_uid + 1);
        let fetches = session
            .uid_fetch(seq, ITEMS)
            .context("fetching new messages")?;
        for f in fetches.iter() {
            // `N:*` always returns the highest UID even if it's old — skip dupes.
            if (f.uid.unwrap_or(0) as i64) <= since_uid {
                continue;
            }
            if let Some(m) = to_stored(f, account, &parser) {
                out.push(m);
            }
        }
    }

    Ok(out)
}

/// Mark a single message (by IMAP UID) as read on the server by adding the
/// `\Seen` flag. Best-effort: the local DB is the source of truth for the UI.
pub fn mark_seen(session: &mut Session, imap_uid: i64) -> Result<()> {
    session.select("INBOX").context("selecting INBOX")?;
    session
        .uid_store(imap_uid.to_string(), "+FLAGS (\\Seen)")
        .context("setting \\Seen flag")?;
    Ok(())
}

fn to_stored(
    f: &imap::types::Fetch,
    account: &str,
    parser: &MessageParser,
) -> Option<StoredMessage> {
    let uid = f.uid? as i64;
    let raw = f.body().or_else(|| f.text())?;
    let msg = parser.parse(raw)?;

    let (from_name, from_addr) = first_address(msg.from());
    let (_, to_addrs) = format_addresses(msg.to());
    let (_, reply_to) = first_address(msg.reply_to());

    let subject = msg.subject().unwrap_or_default().to_string();
    let message_id = msg.message_id().unwrap_or_default().to_string();

    // Authentication-Results carries the receiving server's SPF/DKIM/DMARC
    // verdicts — the strongest signal for spotting spoofed senders.
    let auth_results = msg
        .header("Authentication-Results")
        .and_then(|h| h.as_text())
        .unwrap_or_default()
        .to_string();

    let body_text = msg.body_text(0).map(|c| c.into_owned());
    let body_html = msg.body_html(0).map(|c| c.into_owned());

    // Ensure we always have *some* searchable text.
    let text_for_index = match (&body_text, &body_html) {
        (Some(t), _) => t.clone(),
        (None, Some(h)) => strip_html(h),
        (None, None) => String::new(),
    };
    let snippet = make_snippet(&text_for_index);

    let date_ts = msg
        .date()
        .map(|d| d.to_timestamp())
        .or_else(|| f.internal_date().map(|d| d.timestamp()))
        .unwrap_or(0);

    let mut seen = false;
    let mut flagged = false;
    for flag in f.flags() {
        match flag {
            imap::types::Flag::Seen => seen = true,
            imap::types::Flag::Flagged => flagged = true,
            _ => {}
        }
    }

    Some(StoredMessage {
        imap_uid: uid,
        account: account.to_string(),
        message_id,
        from_name,
        from_addr,
        to_addrs,
        subject,
        date_ts,
        snippet,
        body_text: text_for_index,
        body_html,
        seen,
        flagged,
        reply_to,
        auth_results,
        // Classified by the worker before persisting.
        spam_score: 0.0,
        is_spam: false,
        spam_reasons: String::new(),
    })
}

/// Name + address of the first address in a header.
fn first_address(addr: Option<&Address>) -> (String, String) {
    if let Some(a) = addr {
        if let Some(first) = a.first() {
            let name = first.name().unwrap_or_default().trim().to_string();
            let email = first.address().unwrap_or_default().trim().to_string();
            return (name, email);
        }
    }
    (String::new(), String::new())
}

/// Returns ("Name <email>, ...", "email1, email2, ...").
fn format_addresses(addr: Option<&Address>) -> (String, String) {
    let mut display = Vec::new();
    let mut emails = Vec::new();
    if let Some(a) = addr {
        for item in a.iter() {
            let name = item.name().unwrap_or_default().trim().to_string();
            let email = item.address().unwrap_or_default().trim().to_string();
            if email.is_empty() && name.is_empty() {
                continue;
            }
            if name.is_empty() {
                display.push(email.clone());
            } else {
                display.push(format!("{name} <{email}>"));
            }
            if !email.is_empty() {
                emails.push(email);
            }
        }
    }
    (display.join(", "), emails.join(", "))
}

fn make_snippet(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(160).collect()
}

/// Extremely small HTML-to-text fallback for indexing/snippets (not rendering).
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}
