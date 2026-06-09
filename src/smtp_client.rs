//! Sending mail over SMTP.
//!
//! Supports both Gmail's OAuth2 (SASL XOAUTH2) and plain password / app-password
//! authentication. Transport security is chosen by port: 465 uses implicit TLS,
//! any other port uses STARTTLS.

use anyhow::{Context, Result};
use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::SmtpTransport;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Message, Transport};

/// A message the user wants to send.
pub struct Outgoing {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
}

fn build_message(msg: &Outgoing) -> Result<Message> {
    let from: Mailbox = msg
        .from
        .parse()
        .with_context(|| format!("invalid From address: {}", msg.from))?;
    // Allow a comma-separated recipient list.
    let mut builder = Message::builder().from(from).subject(msg.subject.clone());
    for addr in msg.to.split(',') {
        let addr = addr.trim();
        if addr.is_empty() {
            continue;
        }
        let mailbox: Mailbox = addr
            .parse()
            .with_context(|| format!("invalid recipient address: {addr}"))?;
        builder = builder.to(mailbox);
    }
    let message = builder
        .header(ContentType::TEXT_PLAIN)
        .body(msg.body.clone())
        .context("building message")?;
    Ok(message)
}

/// Build an SMTP transport for the given host/port with the chosen credentials
/// and SASL mechanism.
fn transport(
    host: &str,
    port: u16,
    credentials: Credentials,
    mechanism: Mechanism,
) -> Result<SmtpTransport> {
    let builder = if port == 465 {
        // Implicit TLS (SMTPS).
        SmtpTransport::relay(host).with_context(|| format!("configuring TLS relay {host}"))?
    } else {
        // STARTTLS (e.g. port 587).
        SmtpTransport::starttls_relay(host)
            .with_context(|| format!("configuring STARTTLS relay {host}"))?
    };
    Ok(builder
        .port(port)
        .credentials(credentials)
        .authentication(vec![mechanism])
        .build())
}

/// Send a message using Gmail-style OAuth2 (XOAUTH2).
pub fn send_oauth(
    host: &str,
    port: u16,
    email: &str,
    access_token: &str,
    msg: &Outgoing,
) -> Result<()> {
    let message = build_message(msg)?;
    let credentials = Credentials::new(email.to_string(), access_token.to_string());
    let mailer = transport(host, port, credentials, Mechanism::Xoauth2)?;
    mailer.send(&message).context("sending message (OAuth2)")?;
    Ok(())
}

/// Send a message using username + password (or app-password).
pub fn send_password(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    msg: &Outgoing,
) -> Result<()> {
    let message = build_message(msg)?;
    let credentials = Credentials::new(username.to_string(), password.to_string());
    let mailer = transport(host, port, credentials, Mechanism::Login)?;
    mailer.send(&message).context("sending message")?;
    Ok(())
}
