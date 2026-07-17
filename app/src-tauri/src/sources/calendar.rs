//! Calendar source — the zero-OAuth path: poll a **secret iCal (ICS) URL**
//! every 2 minutes. Google and Outlook both hand these out (README
//! "Integrations" has the click-path); no app registration, no OAuth. URL
//! comes from the keychain (`calendar_ics_url`); the loop idles until set.
//!
//! Events (transitions only):
//!   calendar/meeting_soon    {"summary", "minutes_until"}   ≤5 min before
//!   calendar/meeting_started {"summary"}
//!   calendar/meeting_ended
//!
//! Deliberately skipped, per the plan's false-positive traps: all-day
//! events, TRANSP:TRANSPARENT ("free") events, STATUS:CANCELLED.
//!
//! **Known limitation:** recurring events (RRULE) are not expanded — ICS
//! feeds ship the recurrence rule, not instances, and expanding RFC 5545
//! recurrences correctly is a project of its own. Recurring meetings are
//! ignored (README documents the workaround and the implementation path).

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::collections::HashSet;
use std::time::Duration;

use crate::events::{Bus, Event};
use crate::secrets;

const SOON_MINUTES: i64 = 5;

pub fn spawn(bus: Bus) {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::new();
        let mut current: Option<String> = None; // uid of the meeting we're in
        let mut announced: HashSet<String> = HashSet::new(); // "soon" already fired
        let mut interval = tokio::time::interval(Duration::from_secs(120));
        loop {
            interval.tick().await;
            let Some(url) = secrets::get("calendar_ics_url") else {
                continue;
            };
            let Some(body) = fetch(&client, &url).await else {
                continue;
            };

            let now = Utc::now();
            let events = parse_ics(&body);

            // "Starting soon": the next event within the warning window.
            for ev in &events {
                let mins = (ev.start - now).num_minutes();
                let key = format!("{}@{}", ev.uid, ev.start.timestamp());
                if (0..=SOON_MINUTES).contains(&mins) && !announced.contains(&key) {
                    announced.insert(key);
                    let _ = bus.send(Event::new(
                        "calendar",
                        "meeting_soon",
                        serde_json::json!({ "summary": ev.summary, "minutes_until": mins }),
                    ));
                }
            }
            announced.retain(|k| {
                // Keep only entries whose start is in the future-ish window.
                k.rsplit('@')
                    .next()
                    .and_then(|t| t.parse::<i64>().ok())
                    .map(|t| t > now.timestamp() - 3600)
                    .unwrap_or(false)
            });

            // "In a meeting": the ongoing event with the latest start.
            let ongoing = events
                .iter()
                .filter(|e| e.start <= now && now < e.end)
                .max_by_key(|e| e.start);
            match (ongoing, &current) {
                (Some(ev), cur) if cur.as_deref() != Some(ev.uid.as_str()) => {
                    let _ = bus.send(Event::new(
                        "calendar",
                        "meeting_started",
                        serde_json::json!({ "summary": ev.summary }),
                    ));
                    current = Some(ev.uid.clone());
                }
                (None, Some(_)) => {
                    let _ = bus.send(Event::new(
                        "calendar",
                        "meeting_ended",
                        serde_json::Value::Null,
                    ));
                    current = None;
                }
                _ => {}
            }
        }
    });
}

async fn fetch(client: &reqwest::Client, url: &str) -> Option<String> {
    client.get(url).send().await.ok()?.text().await.ok()
}

struct CalEvent {
    uid: String,
    summary: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Minimal RFC 5545 parse: unfold continuation lines, walk VEVENT blocks,
/// keep timed, opaque, non-cancelled, non-recurring events.
fn parse_ics(body: &str) -> Vec<CalEvent> {
    // Unfold: a line starting with space/tab continues the previous line.
    let mut lines: Vec<String> = Vec::new();
    for raw in body.lines() {
        let raw = raw.trim_end_matches('\r');
        if let Some(cont) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(cont);
                continue;
            }
        }
        lines.push(raw.to_string());
    }

    let mut events = Vec::new();
    let mut cur: Option<(
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        String,
        String,
        bool,
    )> = None; // (start, end, summary, uid, skip)
    for line in &lines {
        match line.as_str() {
            "BEGIN:VEVENT" => cur = Some((None, None, String::new(), String::new(), false)),
            "END:VEVENT" => {
                if let Some((Some(start), Some(end), summary, uid, false)) = cur.take() {
                    events.push(CalEvent {
                        uid,
                        summary,
                        start,
                        end,
                    });
                }
            }
            _ => {
                let Some(state) = cur.as_mut() else { continue };
                let Some((name_params, value)) = line.split_once(':') else {
                    continue;
                };
                let name = name_params.split(';').next().unwrap_or("");
                match name {
                    "DTSTART" => state.0 = parse_dt(name_params, value),
                    "DTEND" => state.1 = parse_dt(name_params, value),
                    "SUMMARY" => state.2 = value.to_string(),
                    "UID" => state.3 = value.to_string(),
                    // The false-positive traps + unexpandable recurrences.
                    "TRANSP" if value == "TRANSPARENT" => state.4 = true,
                    "STATUS" if value == "CANCELLED" => state.4 = true,
                    "RRULE" | "RDATE" => state.4 = true,
                    _ => {}
                }
            }
        }
    }
    events
}

/// `20260712T160000Z` → UTC; `20260712T170000` (incl. TZID=…) → the Mac's
/// local timezone — right whenever your calendar TZ matches your machine.
/// `VALUE=DATE` (all-day) → None, skipping the event.
fn parse_dt(name_params: &str, value: &str) -> Option<DateTime<Utc>> {
    if name_params.contains("VALUE=DATE") && !value.contains('T') {
        // All-day: parse to prove it's a date, then skip.
        NaiveDate::parse_from_str(value, "%Y%m%d").ok();
        return None;
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(Utc.from_utc_datetime(&naive));
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
}
