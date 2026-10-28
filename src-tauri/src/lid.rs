//! The laptop's lid and sleep, for ULTRON's lock screen (Settings, Look & feel, Lock screen: ULTRON).
//!
//! Windows tells ULTRON when the lid closes or opens and when the PC goes to sleep or wakes; the page is told
//! ("lid": "close" | "open" | "sleep" | "wake") and puts its lock screen up as the lid closes (so it's already
//! there when the lid opens, with no glimpse of the desktop first), or as it wakes if that was missed.

use std::sync::atomic::{AtomicI32, Ordering::SeqCst};
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter};
use windows::core::GUID;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{RegisterPowerSettingNotification, RegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING};
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE};

/// GUID_LIDSWITCH_STATE_CHANGE: the lid's state, 0 closed, 1 open (sent once straight away, then on each change).
const LID: GUID = GUID::from_u128(0xba3e0f4d_b817_4094_a2d1_d56379e6a0f3);
/// GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS: the screen's brightness, 0 to 100 (for ULTRON's own indicator)
const BRIGHT: GUID = GUID::from_u128(0x8ffee2c6_2d01_46be_adb9_398addc5b4ff);
static BRIGHT_WAS: AtomicI32 = AtomicI32::new(-1);

static APP: OnceLock<AppHandle> = OnceLock::new();
/// The lid as last told (-1: not told yet).
static LAST: AtomicI32 = AtomicI32::new(-1);

fn tell(what: &str) {
    if let Some(app) = APP.get() {
        crate::log_line(&format!("lid: {what}"));
        let _ = app.emit_to("main", "lid", what);
    }
}

unsafe extern "system" fn on_power(_ctx: *const core::ffi::c_void, kind: u32, setting: *const core::ffi::c_void) -> u32 {
    match kind {
        PBT_APMSUSPEND => tell("sleep"),
        PBT_APMRESUMEAUTOMATIC => tell("wake"),
        PBT_POWERSETTINGCHANGE if !setting.is_null() => {
            let s = unsafe { &*(setting as *const POWERBROADCAST_SETTING) };
            if s.PowerSetting == BRIGHT && s.DataLength >= 4 {
                let v = unsafe { *(s.Data.as_ptr() as *const u32) } as i32;
                // (the first word is only how bright it is now)
                if BRIGHT_WAS.swap(v, SeqCst) >= 0 {
                    if let Some(app) = APP.get() {
                        let _ = app.emit_to("main", "bright", v);
                    }
                }
            }
            if s.PowerSetting == LID && s.DataLength >= 1 {
                let open = unsafe { *s.Data.as_ptr() } != 0;
                let was = LAST.swap(open as i32, SeqCst);
                // (the first word is only how the lid is right now, not a change)
                if was >= 0 && was != open as i32 {
                    tell(if open { "open" } else { "close" });
                }
            }
        }
        _ => {}
    }
    0
}

pub fn start(app: AppHandle) {
    if APP.set(app).is_err() {
        return;
    }
    // (Windows keeps these for as long as ULTRON runs: they're never let go)
    let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
        Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS { Callback: Some(on_power), Context: std::ptr::null_mut() }));
    let h = HANDLE(params as *mut _ as _);
    unsafe {
        if let Err(e) = RegisterSuspendResumeNotification(h, DEVICE_NOTIFY_CALLBACK) {
            crate::log_line(&format!("lid: no sleep notices ({e})"));
        }
        if let Err(e) = RegisterPowerSettingNotification(h, &LID, DEVICE_NOTIFY_CALLBACK) {
            crate::log_line(&format!("lid: no lid notices ({e})"));
        }
        let _ = RegisterPowerSettingNotification(h, &BRIGHT, DEVICE_NOTIFY_CALLBACK);
    }
}
