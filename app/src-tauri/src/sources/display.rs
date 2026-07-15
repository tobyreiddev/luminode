//! Display sleep source (macOS): poll `CGDisplayIsAsleep` on the main
//! display every 5 s. Screen *lock* is a different signal (screenlock.rs) —
//! a display can sleep without locking (energy saver, hot corner), and the
//! strip shouldn't glow at full brightness next to a dark monitor.
//!
//! Events: `system/display_slept`, `system/display_woke` — transitions
//! only. Seeded trigger maps slept → Off until woke.

#[cfg(target_os = "macos")]
pub fn spawn(bus: crate::events::Bus) {
    use crate::events::Event;
    use std::time::Duration;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayIsAsleep(display: u32) -> u32; // boolean_t
    }

    std::thread::Builder::new()
        .name("display-sleep".into())
        .spawn(move || {
            let mut was_asleep = false;
            loop {
                let asleep = unsafe { CGDisplayIsAsleep(CGMainDisplayID()) } != 0;
                if asleep != was_asleep {
                    was_asleep = asleep;
                    let _ = bus.send(Event::new(
                        "system",
                        if asleep { "display_slept" } else { "display_woke" },
                        serde_json::Value::Null,
                    ));
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        })
        .expect("failed to spawn display-sleep thread");
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(_bus: crate::events::Bus) {}
