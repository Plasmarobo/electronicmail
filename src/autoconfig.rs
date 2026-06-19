//! Zero-friction account discovery.
//!
//! Goal: the user types **only their email address**, and we work out the rest
//! — IMAP/SMTP hosts and ports, and whether to sign in with OAuth or a
//! password — exactly the way Thunderbird's autoconfig does.
//!
//! Resolution order (first hit wins):
//!   1. A built-in table of the big providers (instant, offline).
//!   2. Mozilla's ISPDB: `https://autoconfig.thunderbird.net/v1.1/<domain>`.
//!   3. The domain's own autoconfig:
//!      `https://autoconfig.<domain>/mail/config-v1.1.xml` and
//!      `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml`.
//!
//! Gmail uses Google OAuth with a **bring-your-own-client** model: the setup
//! wizard captures the user's own OAuth client id/secret, so no shared client
//! (and no Google verification fee) is needed. A client may optionally be
//! [`oauth_clients`] compiled in as a fallback for private builds.

use anyhow::{Result, anyhow};

/// Optional bundled OAuth client credentials (fallback for private builds).
///
/// The primary path is bring-your-own-client: each account stores its own
/// OAuth client id/secret captured by the setup wizard. For private/internal
/// builds a client may still be injected at **compile time** from the
/// `EM_GOOGLE_CLIENT_ID` / `EM_GOOGLE_CLIENT_SECRET` environment variables so
/// real secrets stay out of source control:
///
/// ```powershell
/// $env:EM_GOOGLE_CLIENT_ID = "xxxx.apps.googleusercontent.com"
/// $env:EM_GOOGLE_CLIENT_SECRET = "yyyy"
/// cargo build --release
/// ```
///
/// A desktop OAuth "secret" is not truly confidential (Google's installed-app
/// model assumes it can be extracted); PKCE is what actually protects the
/// exchange. When these are empty the wizard collects per-account credentials.
pub mod oauth_clients {
    pub const GOOGLE_CLIENT_ID: &str = match option_env!("EM_GOOGLE_CLIENT_ID") {
        Some(v) => v,
        None => "",
    };
    pub const GOOGLE_CLIENT_SECRET: &str = match option_env!("EM_GOOGLE_CLIENT_SECRET") {
        Some(v) => v,
        None => "",
    };

    /// True when a Google OAuth client was compiled into this build.
    pub fn google_configured() -> bool {
        !GOOGLE_CLIENT_ID.is_empty() && !GOOGLE_CLIENT_SECRET.is_empty()
    }
}

/// How the user should authenticate, resolved from their provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// Seamless Google browser sign-in (bundled client).
    OAuthGoogle,
    /// Username + password or provider app-password.
    Password,
}

/// Everything needed to configure an account, discovered from the address.
#[derive(Debug, Clone)]
pub struct MailSettings {
    /// Friendly provider name, e.g. "Gmail" or "Fastmail".
    pub provider_name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub auth: AuthKind,
    /// Guidance shown when an app-password is required.
    pub app_password_hint: Option<String>,
    /// A help link for generating an app-password.
    pub help_url: Option<String>,
}

impl MailSettings {
    fn password(
        provider_name: &str,
        imap_host: &str,
        imap_port: u16,
        smtp_host: &str,
        smtp_port: u16,
    ) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            imap_host: imap_host.to_string(),
            imap_port,
            smtp_host: smtp_host.to_string(),
            smtp_port,
            auth: AuthKind::Password,
            app_password_hint: None,
            help_url: None,
        }
    }

    fn with_hint(mut self, hint: &str, url: &str) -> Self {
        self.app_password_hint = Some(hint.to_string());
        self.help_url = Some(url.to_string());
        self
    }
}

/// The domain portion of an email address, lower-cased.
pub fn domain_of(email: &str) -> String {
    email
        .rsplit('@')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Resolve settings for an email address, trying offline knowledge first and
/// then the network. Returns an error only when nothing recognises the domain.
pub fn discover(email: &str) -> Result<MailSettings> {
    let domain = domain_of(email);
    if domain.is_empty() || !email.contains('@') {
        return Err(anyhow!("that doesn't look like an email address"));
    }
    if let Some(s) = builtin(&domain) {
        return Ok(s);
    }
    if let Some(s) = from_ispdb(&domain) {
        return Ok(s);
    }
    if let Some(s) = from_wellknown(&domain) {
        return Ok(s);
    }
    Err(anyhow!(
        "couldn't automatically find settings for {domain}. You can enter them manually."
    ))
}

/// Built-in settings for the most common providers (no network needed).
fn builtin(domain: &str) -> Option<MailSettings> {
    let s = match domain {
        "gmail.com" | "googlemail.com" => MailSettings {
            provider_name: "Gmail".into(),
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            // Always offer Google OAuth; the user supplies their own OAuth
            // client via the setup wizard ("bring your own client").
            auth: AuthKind::OAuthGoogle,
            app_password_hint: None,
            help_url: None,
        },
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "office365.com"
        | "passport.com" => MailSettings::password(
            "Outlook",
            "outlook.office365.com",
            993,
            "smtp.office365.com",
            587,
        )
        .with_hint(
            "If your Microsoft account uses 2-step verification, create an app password.",
            "https://support.microsoft.com/account-billing/app-passwords",
        ),
        "yahoo.com" | "ymail.com" | "rocketmail.com" => MailSettings::password(
            "Yahoo Mail",
            "imap.mail.yahoo.com",
            993,
            "smtp.mail.yahoo.com",
            465,
        )
        .with_hint(
            "Yahoo requires an app-password generated in your account security settings.",
            "https://help.yahoo.com/kb/SLN15241.html",
        ),
        "aol.com" => MailSettings::password("AOL Mail", "imap.aol.com", 993, "smtp.aol.com", 465)
            .with_hint(
                "AOL requires an app-password from your account security settings.",
                "https://help.aol.com/articles/Create-and-manage-app-password",
            ),
        "icloud.com" | "me.com" | "mac.com" => MailSettings::password(
            "iCloud Mail",
            "imap.mail.me.com",
            993,
            "smtp.mail.me.com",
            587,
        )
        .with_hint(
            "iCloud requires an app-specific password generated at appleid.apple.com.",
            "https://support.apple.com/HT204397",
        ),
        "gmx.com" | "gmx.net" | "gmx.de" => {
            MailSettings::password("GMX", "imap.gmx.com", 993, "mail.gmx.com", 465)
        }
        "fastmail.com" | "fastmail.fm" => MailSettings::password(
            "Fastmail",
            "imap.fastmail.com",
            993,
            "smtp.fastmail.com",
            465,
        )
        .with_hint(
            "Fastmail requires an app-password created in Settings → Privacy & Security.",
            "https://www.fastmail.help/hc/en-us/articles/360058752854",
        ),
        "zoho.com" => {
            MailSettings::password("Zoho Mail", "imap.zoho.com", 993, "smtp.zoho.com", 465)
        }
        "proton.me" | "protonmail.com" | "pm.me" => {
            // Proton requires the local Bridge app; point at it but warn.
            MailSettings::password("Proton Mail", "127.0.0.1", 1143, "127.0.0.1", 1025).with_hint(
                "Proton Mail needs the Proton Mail Bridge app running locally; \
                     use the credentials it provides.",
                "https://proton.me/mail/bridge",
            )
        }
        _ => return None,
    };
    Some(s)
}

/// Query Mozilla's ISPDB for the domain.
fn from_ispdb(domain: &str) -> Option<MailSettings> {
    let url = format!("https://autoconfig.thunderbird.net/v1.1/{domain}");
    let xml = fetch(&url)?;
    parse_autoconfig_xml(&xml)
}

/// Try the domain's own published autoconfig documents.
fn from_wellknown(domain: &str) -> Option<MailSettings> {
    let urls = [
        format!("https://autoconfig.{domain}/mail/config-v1.1.xml"),
        format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml"),
    ];
    for url in urls {
        if let Some(xml) = fetch(&url) {
            if let Some(s) = parse_autoconfig_xml(&xml) {
                return Some(s);
            }
        }
    }
    None
}

fn http_client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .ok()
}

fn fetch(url: &str) -> Option<String> {
    let client = http_client()?;
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().ok()
}

/// Parse the subset of the Thunderbird autoconfig schema we need.
fn parse_autoconfig_xml(xml: &str) -> Option<MailSettings> {
    let incoming = extract_block(xml, "incomingServer", "imap")?;
    let outgoing = extract_block(xml, "outgoingServer", "smtp")?;

    let imap_host = inner(incoming, "hostname")?;
    let imap_port: u16 = inner(incoming, "port")?.parse().ok()?;
    let smtp_host = inner(outgoing, "hostname")?;
    let smtp_port: u16 = inner(outgoing, "port")?.parse().ok()?;

    // We only support Google's OAuth flow; everything else uses a password.
    let oauth_google = incoming.to_ascii_lowercase().contains("oauth2")
        && imap_host.to_ascii_lowercase().contains("google");

    Some(MailSettings {
        provider_name: inner_attr(xml, "emailProvider", "id").unwrap_or_else(|| imap_host.clone()),
        imap_host,
        imap_port,
        smtp_host,
        smtp_port,
        auth: if oauth_google {
            AuthKind::OAuthGoogle
        } else {
            AuthKind::Password
        },
        app_password_hint: None,
        help_url: None,
    })
}

/// Return the inner text of the first `<tag ... type="ty">...</tag>` block.
fn extract_block<'a>(xml: &'a str, tag: &str, ty: &str) -> Option<&'a str> {
    let open_prefix = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    while let Some(rel) = xml[from..].find(&open_prefix) {
        let start = from + rel;
        let tag_end = start + xml[start..].find('>')?;
        let open_tag = &xml[start..=tag_end];
        let matches_type = open_tag.contains(&format!("type=\"{ty}\""))
            || open_tag.contains(&format!("type='{ty}'"));
        if matches_type {
            let end_rel = xml[tag_end..].find(&close)?;
            return Some(&xml[tag_end + 1..tag_end + end_rel]);
        }
        from = tag_end + 1;
    }
    None
}

/// Inner text of the first `<tag>...</tag>`.
fn inner(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = s + xml[s..].find(&close)?;
    Some(xml[s..e].trim().to_string())
}

/// Value of `attr` on the first `<tag ... attr="value" ...>`.
fn inner_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}"))?;
    let tag_end = start + xml[start..].find('>')?;
    let open_tag = &xml[start..=tag_end];
    let needle = format!("{attr}=\"");
    let a = open_tag.find(&needle)? + needle.len();
    let b = a + open_tag[a..].find('"')?;
    Some(open_tag[a..b].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_parsing() {
        assert_eq!(domain_of("Alice@Example.COM"), "example.com");
        assert_eq!(domain_of("bob@gmail.com"), "gmail.com");
    }

    #[test]
    fn rejects_non_email() {
        assert!(discover("not-an-email").is_err());
        assert!(discover("").is_err());
    }

    #[test]
    fn known_providers_resolve_offline() {
        let y = discover("someone@yahoo.com").unwrap();
        assert_eq!(y.imap_host, "imap.mail.yahoo.com");
        assert_eq!(y.auth, AuthKind::Password);
        assert!(y.app_password_hint.is_some());

        let i = discover("me@icloud.com").unwrap();
        assert_eq!(i.smtp_host, "smtp.mail.me.com");
        assert_eq!(i.smtp_port, 587);
    }

    #[test]
    fn gmail_uses_google_oauth() {
        // Gmail always offers Google OAuth now (the user brings their own
        // OAuth client through the setup wizard).
        let g = discover("user@gmail.com").unwrap();
        assert_eq!(g.auth, AuthKind::OAuthGoogle);
        assert_eq!(g.imap_host, "imap.gmail.com");
    }

    #[test]
    fn parses_ispdb_xml() {
        let xml = r#"
        <clientConfig version="1.1">
          <emailProvider id="example.com">
            <incomingServer type="imap">
              <hostname>imap.example.com</hostname>
              <port>993</port>
              <socketType>SSL</socketType>
              <authentication>password-cleartext</authentication>
            </incomingServer>
            <outgoingServer type="smtp">
              <hostname>smtp.example.com</hostname>
              <port>587</port>
              <socketType>STARTTLS</socketType>
            </outgoingServer>
          </emailProvider>
        </clientConfig>"#;
        let s = parse_autoconfig_xml(xml).unwrap();
        assert_eq!(s.provider_name, "example.com");
        assert_eq!(s.imap_host, "imap.example.com");
        assert_eq!(s.imap_port, 993);
        assert_eq!(s.smtp_host, "smtp.example.com");
        assert_eq!(s.smtp_port, 587);
        assert_eq!(s.auth, AuthKind::Password);
    }
}
