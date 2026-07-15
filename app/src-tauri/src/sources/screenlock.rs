//! macOS screen lock/unlock source.
//!
//! Observes the distributed notifications `com.apple.screenIsLocked` /
//! `com.apple.screenIsUnlocked` (no entitlement or user permission needed)
//! and emits ("system", "screen_locked"/"screen_unlocked") onto the bus.
//!
//! CFNotificationCenter isn't wrapped by the core-foundation crate, so the
//! observer registration is hand-declared FFI. The callback runs on a
//! dedicated thread's CFRunLoop; `broadcast::Sender::send` is sync and
//! non-blocking, so calling it from the callback is safe.
//!
//! On non-macOS platforms this module compiles to a no-op — Windows/Linux
//! equivalents (WTS session notifications / logind DBus signals) are future
//! work, tracked in docs/integrations.md.

use crate::events::Bus;

#[cfg(target_os = "macos")]
pub fn spawn(bus: Bus) {
    std::thread::Builder::new()
        .name("screenlock-observer".into())
        .spawn(move || macos::observe(bus))
        .expect("failed to spawn screenlock observer thread");
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(_bus: Bus) {}

#[cfg(target_os = "macos")]
mod macos {
    use crate::events::{Bus, Event};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::os::raw::c_void;

    type CFNotificationCenterRef = *mut c_void;
    type CFDictionaryRef = *const c_void;
    type Callback = extern "C" fn(
        center: CFNotificationCenterRef,
        observer: *mut c_void,
        name: CFStringRef,
        object: *const c_void,
        user_info: CFDictionaryRef,
    );

    // CFNotificationSuspensionBehaviorDeliverImmediately
    const DELIVER_IMMEDIATELY: isize = 4;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
        fn CFNotificationCenterAddObserver(
            center: CFNotificationCenterRef,
            observer: *const c_void,
            callback: Callback,
            name: CFStringRef,
            object: *const c_void,
            suspension_behavior: isize,
        );
        fn CFRunLoopRun();
    }

    extern "C" fn on_notification(
        _center: CFNotificationCenterRef,
        observer: *mut c_void,
        name: CFStringRef,
        _object: *const c_void,
        _user_info: CFDictionaryRef,
    ) {
        // `observer` is the leaked Bus we registered with; the notification
        // name is a static system string, retained by the system (get rule).
        let bus = unsafe { &*(observer as *const Bus) };
        let name = unsafe { CFString::wrap_under_get_rule(name) }.to_string();
        let event_type = if name.ends_with("screenIsLocked") {
            "screen_locked"
        } else {
            "screen_unlocked"
        };
        let _ = bus.send(Event::new("system", event_type, serde_json::Value::Null));
    }

    pub fn observe(bus: Bus) {
        // Leak one Bus clone for the lifetime of the process; the observer
        // registration holds this pointer until exit.
        let observer = Box::into_raw(Box::new(bus)) as *const c_void;
        let locked = CFString::new("com.apple.screenIsLocked");
        let unlocked = CFString::new("com.apple.screenIsUnlocked");
        unsafe {
            let center = CFNotificationCenterGetDistributedCenter();
            CFNotificationCenterAddObserver(
                center,
                observer,
                on_notification,
                locked.as_concrete_TypeRef(),
                std::ptr::null(),
                DELIVER_IMMEDIATELY,
            );
            CFNotificationCenterAddObserver(
                center,
                observer,
                on_notification,
                unlocked.as_concrete_TypeRef(),
                std::ptr::null(),
                DELIVER_IMMEDIATELY,
            );
            CFRunLoopRun(); // parks this thread; callbacks fire here
        }
    }
}
