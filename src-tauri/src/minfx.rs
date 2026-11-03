//! What's left of SYLVER's own minimize animation (a window squeezing up into the notch), now taken out: it had to turn
//! Windows' own minimize animation off while it was on. If SYLVER last left it off (a crash, or a version that had the
//! animation on), Windows' animation is turned back on, once.

use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, ANIMATIONINFO, SPIF_SENDCHANGE, SPI_SETANIMATION};

pub fn heal() {
    if std::fs::remove_file(note()).is_ok() {
        let mut ai = ANIMATIONINFO { cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32, iMinAnimate: 1 };
        unsafe {
            let _ = SystemParametersInfoW(SPI_SETANIMATION, ai.cbSize, Some(&mut ai as *mut _ as *mut core::ffi::c_void), SPIF_SENDCHANGE);
        }
    }
}

fn note() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("SYLVER").join("minimize-animation-was-on")
}
