//! Windows stay below ULTRON's bar, the way they stay below a Mac's menu bar: a window dragged (or resized) up so
//! that its top goes under the bar and the notch is put back just below the bar when you let go of it. Maximized
//! and full-screen windows are left alone (Windows already keeps maximized ones in the space under the bar).

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetAncestor, GetMessageW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, SetWindowPos,
    SystemParametersInfoW, TranslateMessage, EVENT_SYSTEM_MOVESIZEEND, GA_ROOT, MSG, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINEVENT_OUTOFCONTEXT,
};

unsafe extern "system" fn on_moved(_: HWINEVENTHOOK, _: u32, h: HWND, obj: i32, _: i32, _: u32, _: u32) {
    unsafe {
        if obj != 0 || h.is_invalid() || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() || IsZoomed(h).as_bool() {
            return;
        }
        if GetAncestor(h, GA_ROOT) != h {
            return;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return; // (ULTRON's own windows)
        }
        // the space under the bar: the work area (the bar reserves the strip above it; hidden away, it reserves none)
        let mut work = RECT::default();
        if SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_err() || work.top <= 0 {
            return;
        }
        let (mut out, mut seen) = (RECT::default(), RECT::default());
        if GetWindowRect(h, &mut out).is_err() {
            return;
        }
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut seen as *mut _ as _, std::mem::size_of::<RECT>() as u32).is_err() {
            seen = out;
        }
        // (full screen: a game, a video; and windows on another screen below or beside, not under the bar)
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        if seen.left <= 0 && seen.top <= 0 && seen.right >= sw && seen.bottom >= sh {
            return;
        }
        if seen.right <= work.left || seen.left >= work.right || seen.top >= work.top || seen.bottom <= 0 {
            return;
        }
        let down = work.top - seen.top;
        let _ = SetWindowPos(h, None, out.left, out.top + down, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub fn start() {
    std::thread::spawn(|| unsafe {
        let _hook = SetWinEventHook(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND, None, Some(on_moved), 0, 0, WINEVENT_OUTOFCONTEXT);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
}
