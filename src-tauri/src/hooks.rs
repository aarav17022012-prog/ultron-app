//! Low-level hooks that stay hooked. Windows quietly takes away a low-level keyboard or mouse hook that's slow to answer
//! (the PC busy, and the hook's thread not given a turn in time), and says nothing: ULTRON's Alt+Tab or volume keys then
//! stopped answering, and Windows' own came back until ULTRON was started again. Each hook here runs on a thread of the
//! highest priority, so it's given its turn at once, and is put in afresh every 3 seconds in case it was taken away (Windows drops a slow one
//! silently, and its own Alt+Tab came back until the next time).

use std::time::{Duration, Instant};

use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, HOOKPROC, MSG, PM_REMOVE, QS_ALLINPUT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOWS_HOOK_ID,
};

fn set(id: WINDOWS_HOOK_ID, p: HOOKPROC) -> Option<HHOOK> {
    p?;
    unsafe { SetWindowsHookExW(id, p, None, 0) }.ok()
}

/// Runs these hooks (either may be None) for as long as ULTRON runs.
pub fn keep(keyboard: HOOKPROC, mouse: HOOKPROC) {
    std::thread::spawn(move || unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let (mut k, mut m) = (set(WH_KEYBOARD_LL, keyboard), set(WH_MOUSE_LL, mouse));
        let mut at = Instant::now();
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            MsgWaitForMultipleObjects(None, false, 1000, QS_ALLINPUT);
            if at.elapsed() > Duration::from_secs(3) {
                at = Instant::now();
                // (the new one first, then the old one let go: no moment without one)
                for (id, p, h) in [(WH_KEYBOARD_LL, keyboard, &mut k), (WH_MOUSE_LL, mouse, &mut m)] {
                    if let Some(n) = set(id, p) {
                        if let Some(old) = h.replace(n) {
                            let _ = UnhookWindowsHookEx(old);
                        }
                    }
                }
            }
        }
    });
}
