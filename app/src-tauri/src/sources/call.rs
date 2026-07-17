//! "In a call" heuristic (macOS): poll CoreAudio's default input device and
//! every CoreMediaIO camera for `…DeviceIsRunningSomewhere` — true whenever
//! *any* process has the mic or a camera open (Zoom, Meet, Teams, voice
//! memos…). Reading the properties needs no mic/camera permission because
//! nothing is captured. In a call = mic OR camera, so camera-only meetings
//! (mic muted at the OS level) still count.
//!
//! Events: `system/call_started`, `system/call_ended` — transitions only.
//! The plan calls this the fiddliest cross-platform piece; Windows/Linux
//! equivalents belong here behind the same two event types.

#[cfg(target_os = "macos")]
pub fn spawn(bus: crate::events::Bus) {
    use crate::events::Event;
    use std::time::Duration;

    std::thread::Builder::new()
        .name("call-detect".into())
        .spawn(move || {
            let mut was_in_use = false;
            loop {
                let in_use =
                    macos::mic_in_use().unwrap_or(false) || macos::camera_in_use().unwrap_or(false);
                if in_use != was_in_use {
                    was_in_use = in_use;
                    let _ = bus.send(Event::new(
                        "system",
                        if in_use { "call_started" } else { "call_ended" },
                        serde_json::Value::Null,
                    ));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("failed to spawn call-detect thread");
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(_bus: crate::events::Bus) {}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;

    // FourCC constants from AudioHardware.h.
    const SYSTEM_OBJECT: u32 = 1; // kAudioObjectSystemObject
    const DEFAULT_INPUT_DEVICE: u32 = u32::from_be_bytes(*b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const IS_RUNNING_SOMEWHERE: u32 = u32::from_be_bytes(*b"gone"); // kAudioDevicePropertyDeviceIsRunningSomewhere
    const SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob"); // kAudioObjectPropertyScopeGlobal

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32, // 0 = kAudioObjectPropertyElementMain
    }

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyData(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    fn get_u32(object: u32, selector: u32) -> Option<u32> {
        let address = AudioObjectPropertyAddress {
            selector,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                &mut value as *mut u32 as *mut c_void,
            )
        };
        (status == 0).then_some(value)
    }

    pub fn mic_in_use() -> Option<bool> {
        let device = get_u32(SYSTEM_OBJECT, DEFAULT_INPUT_DEVICE)?;
        if device == 0 {
            return Some(false); // no input device at all
        }
        Some(get_u32(device, IS_RUNNING_SOMEWHERE)? != 0)
    }

    // --- cameras (CoreMediaIO — same property model, slightly different
    // C signature: an extra dataUsed out-param) ---

    const CMIO_HARDWARE_PROPERTY_DEVICES: u32 = u32::from_be_bytes(*b"dev#");

    #[link(name = "CoreMediaIO", kind = "framework")]
    extern "C" {
        fn CMIOObjectGetPropertyDataSize(
            object_id: u32,
            address: *const AudioObjectPropertyAddress, // same layout as CMIO's
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: *mut u32,
        ) -> i32;
        fn CMIOObjectGetPropertyData(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier_data: *const c_void,
            data_size: u32,
            data_used: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    pub fn camera_in_use() -> Option<bool> {
        let devices_addr = AudioObjectPropertyAddress {
            selector: CMIO_HARDWARE_PROPERTY_DEVICES,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut size: u32 = 0;
        let status = unsafe {
            CMIOObjectGetPropertyDataSize(
                SYSTEM_OBJECT, // kCMIOObjectSystemObject is also 1
                &devices_addr,
                0,
                std::ptr::null(),
                &mut size,
            )
        };
        if status != 0 || size == 0 {
            return Some(false); // no cameras (or CMIO unhappy) — not in use
        }
        let count = (size as usize) / std::mem::size_of::<u32>();
        let mut ids = vec![0u32; count];
        let mut used: u32 = 0;
        let status = unsafe {
            CMIOObjectGetPropertyData(
                SYSTEM_OBJECT,
                &devices_addr,
                0,
                std::ptr::null(),
                size,
                &mut used,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            return None;
        }
        let running_addr = AudioObjectPropertyAddress {
            selector: IS_RUNNING_SOMEWHERE, // same 'gone' FourCC in CMIO
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        for id in ids {
            let mut value: u32 = 0;
            let mut value_used: u32 = 0;
            let status = unsafe {
                CMIOObjectGetPropertyData(
                    id,
                    &running_addr,
                    0,
                    std::ptr::null(),
                    std::mem::size_of::<u32>() as u32,
                    &mut value_used,
                    &mut value as *mut u32 as *mut c_void,
                )
            };
            if status == 0 && value != 0 {
                return Some(true);
            }
        }
        Some(false)
    }
}
