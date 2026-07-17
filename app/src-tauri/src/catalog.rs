use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationDescriptor {
    pub source: &'static str,
    pub name: &'static str,
    pub setup: &'static str,
    pub events: &'static [&'static str],
}

pub fn all() -> Vec<IntegrationDescriptor> {
    vec![
        IntegrationDescriptor {
            source: "cli",
            name: "Terminal",
            setup: "lightctl",
            events: &[
                "progress",
                "progress_done",
                "run_started",
                "run_succeeded",
                "run_failed",
            ],
        },
        IntegrationDescriptor {
            source: "codex",
            name: "Codex",
            setup: "command hooks",
            events: &["active", "stopped"],
        },
        IntegrationDescriptor {
            source: "claude",
            name: "Claude Code",
            setup: "hooks and status line",
            events: &["active", "stopped", "usage"],
        },
        IntegrationDescriptor {
            source: "calendar",
            name: "Calendar",
            setup: "secret HTTPS iCalendar URL",
            events: &["meeting_soon", "meeting_started", "meeting_ended"],
        },
        IntegrationDescriptor {
            source: "slack",
            name: "Slack",
            setup: "user token",
            events: &[
                "status_set",
                "status_cleared",
                "presence_active",
                "presence_away",
            ],
        },
        IntegrationDescriptor {
            source: "system",
            name: "macOS",
            setup: "automatic",
            events: &[
                "screen_locked",
                "screen_unlocked",
                "display_slept",
                "display_woke",
                "call_started",
                "call_ended",
            ],
        },
        IntegrationDescriptor {
            source: "time",
            name: "Schedules",
            setup: "in app",
            events: &["idle_changed"],
        },
        IntegrationDescriptor {
            source: "device",
            name: "Luminode device",
            setup: "USB",
            events: &["connected", "disconnected", "firmware_error", "usb_reset"],
        },
    ]
}
