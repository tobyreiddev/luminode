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
//! Common daily and weekly recurring events are expanded in a bounded rolling
//! window. Complex BYDAY/RDATE rules fail closed until a full RFC 5545 engine
//! is introduced.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::collections::HashSet;
use std::time::Duration;

use crate::events::{Bus, Event};
use crate::secrets;

const SOON_MINUTES: i64 = 5;
const MAX_ICS_BYTES: usize = 5 * 1024 * 1024;

pub fn spawn(bus: Bus, health: crate::health::HealthRegistry) {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("calendar HTTP client configuration is valid");
        let mut current: Option<String> = None; // uid of the meeting we're in
        let mut announced: HashSet<String> = HashSet::new(); // "soon" already fired
        let mut interval = tokio::time::interval(Duration::from_secs(120));
        loop {
            interval.tick().await;
            let Some(url) = secrets::get("calendar_ics_url") else {
                health.idle("calendar", "Add an iCalendar URL to connect");
                continue;
            };
            let Some(body) = fetch(&client, &url).await else {
                health.failure("calendar", "Calendar fetch failed");
                continue;
            };
            health.success("calendar");

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
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let mut response = client
        .get(parsed)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_ICS_BYTES as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_ICS_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).ok()
}

struct CalEvent {
    uid: String,
    summary: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Default)]
struct CalEventDraft {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    summary: String,
    uid: String,
    skip: bool,
    rrule: Option<String>,
}

/// Minimal RFC 5545 parse: unfold continuation lines, walk VEVENT blocks,
/// keep timed, opaque, non-cancelled events and common recurrences.
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
    let mut cur: Option<CalEventDraft> = None;
    for line in &lines {
        match line.as_str() {
            "BEGIN:VEVENT" => cur = Some(CalEventDraft::default()),
            "END:VEVENT" => {
                if let Some(draft) = cur.take() {
                    if let (Some(start), Some(end), false) = (draft.start, draft.end, draft.skip) {
                        let event = CalEvent {
                            uid: draft.uid,
                            summary: draft.summary,
                            start,
                            end,
                        };
                        if let Some(rule) = draft.rrule {
                            events.extend(expand_recurrence(event, &rule));
                        } else {
                            events.push(event);
                        }
                    }
                }
            }
            _ => {
                let Some(state) = cur.as_mut() else { continue };
                let Some((name_params, value)) = line.split_once(':') else {
                    continue;
                };
                let name = name_params.split(';').next().unwrap_or("");
                match name {
                    "DTSTART" => state.start = parse_dt(name_params, value),
                    "DTEND" => state.end = parse_dt(name_params, value),
                    "SUMMARY" => state.summary = value.to_string(),
                    "UID" => state.uid = value.to_string(),
                    // The false-positive traps + unexpandable recurrences.
                    "TRANSP" if value == "TRANSPARENT" => state.skip = true,
                    "STATUS" if value == "CANCELLED" => state.skip = true,
                    "RRULE" => state.rrule = Some(value.to_string()),
                    "RDATE" => state.skip = true,
                    _ => {}
                }
            }
        }
    }
    events
}

fn expand_recurrence(event: CalEvent, rule: &str) -> Vec<CalEvent> {
    let fields: std::collections::HashMap<&str, &str> = rule
        .split(';')
        .filter_map(|part| part.split_once('='))
        .collect();
    let days = match fields.get("FREQ").copied() {
        Some("DAILY") => 1,
        Some("WEEKLY") if !fields.contains_key("BYDAY") => 7,
        _ => return Vec::new(),
    };
    let interval = fields
        .get("INTERVAL")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| (1..=365).contains(v))
        .unwrap_or(1);
    let count = fields
        .get("COUNT")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5_000)
        .min(5_000);
    let until = fields.get("UNTIL").and_then(|v| parse_dt("UNTIL", v));
    let now = Utc::now();
    let window_start = now - chrono::Duration::days(1);
    let window_end = now + chrono::Duration::days(90);
    let duration = event.end - event.start;
    let step = chrono::Duration::days(days * interval);
    let mut start = event.start;
    let mut out = Vec::new();
    for _ in 0..count {
        if until.is_some_and(|limit| start > limit) || start > window_end {
            break;
        }
        if start + duration >= window_start {
            out.push(CalEvent {
                uid: format!("{}@{}", event.uid, start.timestamp()),
                summary: event.summary.clone(),
                start,
                end: start + duration,
            });
        }
        start += step;
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_timed_event_and_unfolds_summary() {
        let events = parse_ics("BEGIN:VEVENT\nUID:1\nDTSTART:20260717T010000Z\nDTEND:20260717T020000Z\nSUMMARY:Design \n review\nEND:VEVENT");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Design review");
    }

    #[test]
    fn skips_cancelled_free_and_all_day_events() {
        for extra in ["STATUS:CANCELLED", "TRANSP:TRANSPARENT"] {
            let body = format!("BEGIN:VEVENT\nUID:1\nDTSTART:20260717T010000Z\nDTEND:20260717T020000Z\n{extra}\nEND:VEVENT");
            assert!(parse_ics(&body).is_empty());
        }
        assert!(parse_ics("BEGIN:VEVENT\nUID:1\nDTSTART;VALUE=DATE:20260717\nDTEND;VALUE=DATE:20260718\nEND:VEVENT").is_empty());
    }

    #[test]
    fn expands_bounded_daily_recurrence() {
        let start = (Utc::now() - chrono::Duration::days(2)).format("%Y%m%dT%H%M%SZ");
        let end = (Utc::now() - chrono::Duration::days(2) + chrono::Duration::hours(1))
            .format("%Y%m%dT%H%M%SZ");
        let body = format!("BEGIN:VEVENT\nUID:daily\nDTSTART:{start}\nDTEND:{end}\nRRULE:FREQ=DAILY;COUNT=10\nEND:VEVENT");
        assert!(!parse_ics(&body).is_empty());
    }
}
