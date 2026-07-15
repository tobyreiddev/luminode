//! Event sources. Each source is a task/thread that pushes uniform
//! [`crate::events::Event`]s onto the bus and knows nothing about rules or
//! animations.
//!
//! Implemented today:
//! * `lightctl` — unix socket fed by the `lightctl` CLI (progress bars,
//!   command wrappers, arbitrary script events, the Claude Code bridge).
//! * `screenlock` — macOS screen lock/unlock distributed notifications.
//! * `call` — macOS mic/camera-in-use polling → system/call_started|ended.
//! * `display` — macOS display sleep → system/display_slept|woke.
//! * `schedule` — user-defined daily times → time/* events or idle swaps.
//! * `slack` — status/presence polling (token in keychain).
//! * `calendar` — secret iCal URL polling (URL in keychain).
//!
//! Planned (see docs/integrations.md): MS Teams presence, Gmail/IMAP
//! new-mail, Windows/Linux screen lock. Each should live in its own module
//! here, own its polling/webhook loop, and emit events — priority handling
//! stays in the trigger engine.

pub mod calendar;
pub mod call;
pub mod display;
pub mod lightctl;
pub mod schedule;
pub mod screenlock;
pub mod slack;
