//! Google Calendar integration (read + write) via the Calendar API v3.
//!
//! Uses a short-lived OAuth2 access token (the same one minted for Gmail; the
//! calendar scope is requested at sign-in). All calls hit the user's `primary`
//! calendar.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const BASE: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";

/// A calendar event in a display-friendly shape.
#[derive(Debug, Clone)]
pub struct CalEvent {
    pub summary: String,
    pub location: String,
    /// Start as an RFC3339 datetime or a YYYY-MM-DD date (all-day).
    pub start: String,
    pub end: String,
    pub all_day: bool,
}

/// A new event to create.
pub struct NewEvent {
    pub summary: String,
    pub description: String,
    pub location: String,
    /// RFC3339 timestamps, e.g. "2026-06-10T14:00:00-07:00".
    pub start_rfc3339: String,
    pub end_rfc3339: String,
}

// --- Wire types (subset of the API) ---

#[derive(Deserialize)]
struct EventsResponse {
    #[serde(default)]
    items: Vec<ApiEvent>,
}

#[derive(Deserialize)]
struct ApiEvent {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    start: Option<ApiTime>,
    #[serde(default)]
    end: Option<ApiTime>,
}

#[derive(Deserialize, Serialize, Default)]
struct ApiTime {
    #[serde(skip_serializing_if = "Option::is_none", rename = "dateTime")]
    date_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
}

#[derive(Serialize)]
struct CreateBody {
    summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    location: String,
    start: ApiTime,
    end: ApiTime,
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

fn to_cal_event(e: ApiEvent) -> CalEvent {
    let (start, start_all_day) = time_to_string(e.start);
    let (end, _) = time_to_string(e.end);
    CalEvent {
        summary: e.summary.unwrap_or_else(|| "(no title)".to_string()),
        location: e.location.unwrap_or_default(),
        start,
        end,
        all_day: start_all_day,
    }
}

fn time_to_string(t: Option<ApiTime>) -> (String, bool) {
    match t {
        Some(ApiTime {
            date_time: Some(dt),
            ..
        }) => (dt, false),
        Some(ApiTime { date: Some(d), .. }) => (d, true),
        _ => (String::new(), false),
    }
}

/// List upcoming events (from now), soonest first.
pub fn list_upcoming(
    access_token: &str,
    time_min_rfc3339: &str,
    max: u32,
) -> Result<Vec<CalEvent>> {
    let client = http_client()?;
    let resp = client
        .get(BASE)
        .bearer_auth(access_token)
        .query(&[
            ("timeMin", time_min_rfc3339),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", &max.to_string()),
        ])
        .send()
        .context("requesting calendar events")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("calendar list failed ({status}): {text}");
    }

    let parsed: EventsResponse = resp.json().context("parsing calendar response")?;
    Ok(parsed.items.into_iter().map(to_cal_event).collect())
}

/// Create an event on the primary calendar.
pub fn create_event(access_token: &str, event: &NewEvent) -> Result<CalEvent> {
    let client = http_client()?;
    let body = CreateBody {
        summary: event.summary.clone(),
        description: event.description.clone(),
        location: event.location.clone(),
        start: ApiTime {
            date_time: Some(event.start_rfc3339.clone()),
            date: None,
        },
        end: ApiTime {
            date_time: Some(event.end_rfc3339.clone()),
            date: None,
        },
    };

    let resp = client
        .post(BASE)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .context("creating calendar event")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("calendar create failed ({status}): {text}");
    }

    let created: ApiEvent = resp.json().context("parsing created event")?;
    Ok(to_cal_event(created))
}
