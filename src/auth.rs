//! Gmail OAuth2 using the loopback (installed-app) flow with PKCE.
//!
//! The OAuth client id/secret are supplied **per account** ("bring your own
//! client"): the setup wizard captures them from the user's own Google Cloud
//! project, so no shared client — and no Google verification fee — is required.
//! We open the system browser, capture the redirect on
//! `http://127.0.0.1:<ephemeral-port>`, then exchange the code for tokens.

use anyhow::{Context, Result, bail};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    RefreshToken, Scope, TokenResponse, TokenUrl,
};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Scopes we request: IMAP/SMTP mail access plus Google Calendar read/write.
const SCOPES: &[&str] = &[
    "https://mail.google.com/",
    "https://www.googleapis.com/auth/calendar",
];

/// Result of a successful sign-in.
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Extract `(client_id, client_secret)` from the JSON that Google's Cloud
/// Console hands out when you download an OAuth client (the
/// `client_secret_*.json` file). Handles both the desktop (`installed`) and
/// web (`web`) wrappers, as well as a bare `{ "client_id", "client_secret" }`.
pub fn parse_oauth_client_json(text: &str) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Client {
        client_id: Option<String>,
        client_secret: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Wrapper {
        installed: Option<Client>,
        web: Option<Client>,
    }

    let value = serde_json::from_str::<Wrapper>(text).ok()?;
    let client = value.installed.or(value.web).or_else(|| {
        // Fall back to a bare client object with no wrapper.
        serde_json::from_str::<Client>(text).ok()
    })?;
    let id = client.client_id?;
    let secret = client.client_secret?;
    if id.trim().is_empty() || secret.trim().is_empty() {
        return None;
    }
    Some((id.trim().to_string(), secret.trim().to_string()))
}

/// Best-effort extraction of OAuth client credentials from arbitrary pasted
/// text: a downloaded JSON blob, or a loose paste containing the client id and
/// secret (e.g. copied straight from the console).
pub fn parse_client_credentials(text: &str) -> Option<(String, String)> {
    if let Some(pair) = parse_oauth_client_json(text) {
        return Some(pair);
    }
    let tokens = || text.split(|c: char| c.is_whitespace() || "\"',{}[]:".contains(c));
    let id = tokens().find(|t| t.ends_with(".apps.googleusercontent.com"))?;
    let secret = tokens().find(|t| t.starts_with("GOCSPX-"))?;
    Some((id.to_string(), secret.to_string()))
}

type ConfiguredClient = oauth2::Client<
    oauth2::basic::BasicErrorResponse,
    oauth2::basic::BasicTokenResponse,
    oauth2::basic::BasicTokenIntrospectionResponse,
    oauth2::StandardRevocableToken,
    oauth2::basic::BasicRevocationErrorResponse,
    oauth2::EndpointSet,    // auth
    oauth2::EndpointNotSet, // device auth
    oauth2::EndpointNotSet, // introspection
    oauth2::EndpointNotSet, // revocation
    oauth2::EndpointSet,    // token
>;

fn build_client(
    client_id: &str,
    client_secret: &str,
    redirect: Option<RedirectUrl>,
) -> Result<ConfiguredClient> {
    let mut client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_client_secret(ClientSecret::new(client_secret.to_string()))
        .set_auth_uri(AuthUrl::new(AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.to_string())?);
    if let Some(redirect) = redirect {
        client = client.set_redirect_uri(redirect);
    }
    Ok(client)
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::ClientBuilder::new()
        // Per oauth2 guidance: disable redirects to mitigate SSRF.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

/// Run the interactive sign-in flow. Blocks until the user completes (or aborts)
/// the browser consent screen. `open_browser` is invoked with the consent URL.
pub fn interactive_login(
    client_id: &str,
    client_secret: &str,
    open_browser: impl FnOnce(&str),
) -> Result<Tokens> {
    // Bind first so we know which ephemeral port to register as the redirect.
    let listener = TcpListener::bind("127.0.0.1:0").context("binding loopback listener")?;
    let port = listener.local_addr()?.port();
    let redirect = RedirectUrl::new(format!("http://127.0.0.1:{port}"))?;

    let client = build_client(client_id, client_secret, Some(redirect))?;

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let mut auth = client.authorize_url(CsrfToken::new_random);
    for scope in SCOPES {
        auth = auth.add_scope(Scope::new((*scope).to_string()));
    }
    let (auth_url, csrf_token) = auth
        // `offline` + `consent` ensures Google returns a refresh token.
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

    open_browser(auth_url.as_str());

    let (code, state) = wait_for_redirect(&listener)?;
    if state.secret() != csrf_token.secret() {
        bail!("OAuth state mismatch — possible CSRF, aborting");
    }

    let http = http_client()?;
    let token = client
        .exchange_code(code)
        .set_pkce_verifier(pkce_verifier)
        .request(&http)
        .context("exchanging authorization code for tokens")?;

    Ok(Tokens {
        access_token: token.access_token().secret().clone(),
        refresh_token: token.refresh_token().map(|t| t.secret().clone()),
    })
}

/// Exchange a stored refresh token for a fresh access token (no browser).
pub fn refresh(client_id: &str, client_secret: &str, refresh_token: &str) -> Result<Tokens> {
    let client = build_client(client_id, client_secret, None)?;
    let http = http_client()?;
    let token = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request(&http)
        .context("refreshing access token")?;
    Ok(Tokens {
        access_token: token.access_token().secret().clone(),
        // Google usually omits a new refresh token; keep the existing one.
        refresh_token: token
            .refresh_token()
            .map(|t| t.secret().clone())
            .or_else(|| Some(refresh_token.to_string())),
    })
}

/// Block on the loopback socket until Google redirects back with `?code=...`.
fn wait_for_redirect(listener: &TcpListener) -> Result<(AuthorizationCode, CsrfToken)> {
    for stream in listener.incoming() {
        let mut stream = stream.context("accepting redirect connection")?;
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;

        // Request line: "GET /?code=...&state=... HTTP/1.1"
        let Some(path) = request_line.split_whitespace().nth(1) else {
            // Not a well-formed request (e.g. a probe) — keep waiting.
            continue;
        };

        let url = url::Url::parse(&format!("http://localhost{path}"))?;
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => {
                    respond(
                        &mut stream,
                        "Authentication failed. You can close this tab.",
                    );
                    bail!("Google returned an OAuth error: {v}");
                }
                _ => {}
            }
        }

        // Browsers often hit the loopback with stray requests (favicon, etc.)
        // before the real redirect. Ignore anything without a code and keep
        // listening rather than giving up.
        let (Some(code), Some(state)) = (code, state) else {
            respond(&mut stream, "Waiting for sign-in to complete…");
            continue;
        };

        respond(
            &mut stream,
            "Signed in to electronicmail. You can close this tab and return to the app.",
        );
        return Ok((AuthorizationCode::new(code), CsrfToken::new(state)));
    }
    bail!("listener closed before receiving a redirect")
}

fn respond(stream: &mut std::net::TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\">\
         <h2>electronicmail</h2><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
