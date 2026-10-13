//! Live liquid glass: ULTRON's glass shows what's really behind it (your windows, not just the
//! wallpaper). A few times a second the area under ULTRON's overlay is captured with ULTRON itself
//! left out (Windows' "exclude from capture", switched on only for that instant, so your own
//! screenshots still show ULTRON), shrunk, blurred, and handed to the page as the glass's backdrop.
//! It runs slowly when only the bar shows and not at all while a full-screen app hides the bar.
//!
//! While a panel or the floating lyrics are open the glass is live: Windows hands over each new picture
//! of the screen as it is drawn (the service screen recorders use), so what's behind the glass moves with
//! it instead of a few times a second. ULTRON is left out of captures for as long as that lasts, except
//! when a screenshot key is pressed or ULTRON's own recorder is running.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};

/// How often to refresh, in ms (0 = paused). Set by the page from what's on screen.
static EVERY: AtomicU64 = AtomicU64::new(700);
const W: u32 = 360; // the backdrop's width: plenty for something this blurred
const FAST: u64 = 120; // a refresh this often (ms) or oftener means "live": a panel or the floating lyrics are showing

pub fn set_rate(ms: u64) {
    EVERY.store(ms, Ordering::SeqCst);
}

/// One box-blur pass along rows (or columns), using running sums.
fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize, rows: bool) {
    let (lines, len) = if rows { (h, w) } else { (w, h) };
    let mut pre = vec![0f32; len + 1];
    for l in 0..lines {
        let at = |i: usize| if rows { (l * w + i) * 3 } else { (i * w + l) * 3 };
        for c in 0..3 {
            for i in 0..len {
                pre[i + 1] = pre[i] + src[at(i) + c];
            }
            for i in 0..len {
                let (lo, hi) = (i.saturating_sub(r), (i + r).min(len - 1));
                dst[at(i) + c] = (pre[hi + 1] - pre[lo]) / (hi - lo + 1) as f32;
            }
        }
    }
}

/// A fast blur: three box-blur passes each way look like a Gaussian.
fn blur(px: &mut [f32], w: usize, h: usize, r: usize) {
    let mut tmp = vec![0f32; px.len()];
    for _ in 0..3 {
        box_pass(px, &mut tmp, w, h, r, true);
        box_pass(&tmp, px, w, h, r, false);
    }
}

/// Copy a part of the screen straight into a small bitmap (Windows averages the pixels as it shrinks).
fn grab_small(r: RECT, w: i32, h: i32) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let sdc = GetDC(None);
        let mdc = CreateCompatibleDC(Some(sdc));
        let bmp = CreateCompatibleBitmap(sdc, w, h);
        let old = SelectObject(mdc, bmp.into());
        SetStretchBltMode(mdc, HALFTONE);
        let _ = SetBrushOrgEx(mdc, 0, 0, None);
        let ok = StretchBlt(mdc, 0, 0, w, h, Some(sdc), r.left, r.top, r.right - r.left, r.bottom - r.top, SRCCOPY).as_bool();
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mdc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS);
        SelectObject(mdc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mdc);
        ReleaseDC(None, sdc);
        (ok && lines > 0).then_some(buf)
    }
}

/// While the screen is being recorded the glass keeps the picture it has: ULTRON has to stay in the
/// recording, steadily, and seeing what's behind it means leaving it out of captures.
static HOLD: AtomicBool = AtomicBool::new(false);
static BUSY: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// ULTRON's window (for the threads that only put it back into captures); until when it stays in them
/// because a screenshot key was just pressed; and whether the live feed is running.
static WIN: AtomicIsize = AtomicIsize::new(0);
static SNAP_UNTIL: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicBool = AtomicBool::new(false);
/// Another program is recording or streaming the screen (others_watch).
static OTHERS: AtomicBool = AtomicBool::new(false);
/// The page wants a fresh backdrop now (the wallpaper changed, the bar has just come back down).
static KICK: AtomicBool = AtomicBool::new(false);

pub fn kick() {
    KICK.store(true, Ordering::SeqCst);
}

fn ticks() -> u64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

fn paused() -> bool {
    HOLD.load(Ordering::SeqCst) || OTHERS.load(Ordering::SeqCst) || ticks() < SNAP_UNTIL.load(Ordering::SeqCst)
}

/// Leave ULTRON out of screen captures, or put it back.
fn exclude(on: bool) {
    let v = WIN.load(Ordering::SeqCst);
    if v != 0 {
        unsafe {
            let _ = SetWindowDisplayAffinity(HWND(v as *mut core::ffi::c_void), if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE });
        }
    }
}

pub fn hold(on: bool) {
    HOLD.store(on, Ordering::SeqCst);
    let _wait = BUSY.lock(); // (a refresh already under way finishes first)
    if on {
        exclude(false);
        std::thread::sleep(Duration::from_millis(50)); // (ULTRON is back in the picture before the recording's first frame)
    }
}

/// Keys that start a screenshot or a recording in Windows, the Snipping Tool, the Game Bar and NVIDIA's
/// overlay (PrtSc, Win+Shift+S, Win+Shift+R, Win+Alt+R, Win+Alt+G, Win+G, Alt+F9, Alt+F10, Alt+Z): ULTRON goes
/// straight back into captures and stays there for a while, so it is in what's being taken and the recorder
/// isn't put off by a window that hides from it. By then the recording itself is noticed (others_watch).
fn snap_watch() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F10, VK_F9, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_SNAPSHOT};
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(20));
        let down = |k: i32| unsafe { GetAsyncKeyState(k) } < 0;
        let (win, shift, alt) = (down(VK_LWIN.0 as i32) || down(VK_RWIN.0 as i32), down(VK_SHIFT.0 as i32), down(VK_MENU.0 as i32));
        let key = |c: u8| down(c as i32);
        let hit = down(VK_SNAPSHOT.0 as i32)
            || (win && shift && (key(b'S') || key(b'R')))
            || (win && alt && (key(b'R') || key(b'G')))
            || (win && key(b'G'))
            || (alt && (down(VK_F9.0 as i32) || down(VK_F10.0 as i32) || key(b'Z')));
        if hit {
            SNAP_UNTIL.store(ticks() + 15_000, Ordering::SeqCst);
            exclude(false);
        }
    });
}

/// The graphics card's video encoder, asked through NVIDIA's own library (nvml.dll, which comes with the
/// driver): how many programs are compressing video right now. Zero when there is no NVIDIA card.
mod nvenc {
    use std::sync::OnceLock;
    type Stats = unsafe extern "C" fn(*mut core::ffi::c_void, *mut u32, *mut u32, *mut u32) -> i32;
    struct Lib {
        device: usize,
        stats: Stats,
    }
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();

    pub fn sessions() -> u32 {
        let lib = LIB.get_or_init(|| unsafe {
            use windows::core::{s, w};
            use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
            let h = LoadLibraryW(w!("nvml.dll")).ok()?;
            let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(GetProcAddress(h, s!("nvmlInit_v2"))?);
            let by_index: unsafe extern "C" fn(u32, *mut *mut core::ffi::c_void) -> i32 = std::mem::transmute(GetProcAddress(h, s!("nvmlDeviceGetHandleByIndex_v2"))?);
            let stats: Stats = std::mem::transmute(GetProcAddress(h, s!("nvmlDeviceGetEncoderStats"))?);
            if init() != 0 {
                return None;
            }
            let mut device = std::ptr::null_mut();
            (by_index(0, &mut device) == 0).then_some(Lib { device: device as usize, stats })
        });
        let Some(l) = lib else { return 0 };
        let (mut n, mut fps, mut wait) = (0u32, 0u32, 0u32);
        if unsafe { (l.stats)(l.device as *mut _, &mut n, &mut fps, &mut wait) } == 0 { n } else { 0 }
    }
}

/// Is another program recording or streaming the screen? Two signs: the graphics card's video encoder is at
/// work (NVIDIA's recorder, the Snipping Tool, OBS, Discord and video calls all compress what they capture
/// with it), or a recording program that doesn't use it is running. While either holds, ULTRON never hides
/// from captures: its glass keeps the backdrop it has, and the recording shows ULTRON steadily. (Hiding from
/// captures, even for a moment, made ULTRON blink in other programs' recordings, and can stop NVIDIA's
/// recorder outright: it treats a window that hides as protected content.)
fn others_watch() {
    const RECORDERS: [&str; 10] = ["obs64.exe", "obs32.exe", "sharex.exe", "bdcam.exe", "camrecorder.exe", "bcastdvr.exe", "screenrec.exe", "loom.exe", "action.exe", "captura.exe"];
    std::thread::spawn(|| {
        let (mut round, mut by_name, mut told) = (0u32, false, false);
        loop {
            std::thread::sleep(Duration::from_millis(400));
            round = round.wrapping_add(1);
            if round % 5 == 0 {
                by_name = crate::kids::processes().iter().any(|p| RECORDERS.contains(&p.2.as_str()));
            }
            let on = by_name || nvenc::sessions() > 0;
            if on != OTHERS.swap(on, Ordering::SeqCst) && on {
                exclude(false);
                if !told {
                    told = true;
                    crate::log_line("live glass: something is recording the screen, so ULTRON stays in the picture and the glass keeps the backdrop it has");
                }
            }
        }
    });
}

fn window(app: &AppHandle) -> Option<(HWND, RECT)> {
    let main = app.get_webview_window("main")?;
    let h = crate::hit::hwnd_of(&main)?;
    WIN.store(h.0 as isize, Ordering::SeqCst);
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    ((r.right - r.left) >= 10 && (r.bottom - r.top) >= 10).then_some((h, r))
}

/// The small picture's size for a window this big.
fn small_size(r: RECT) -> (usize, usize) {
    let k = ((r.right - r.left) as f32 / W as f32).max(1.0);
    (W as usize, (((r.bottom - r.top) as f32 / k).round() as usize).max(1))
}

/// A small picture of the screen (BGRA), blurred and toned, as a JPEG data URL.
fn finish(shot: &[u8], w: usize, hh: usize) -> Option<String> {
    let mut small: Vec<f32> = Vec::with_capacity(w * hh * 3);
    for p in shot.chunks_exact(4) {
        small.extend_from_slice(&[p[2] as f32, p[1] as f32, p[0] as f32]);
    }
    blur(&mut small, w, hh, 4);
    let mut bytes: Vec<u8> = small.iter().map(|v| v.round().clamp(0.0, 255.0) as u8).collect();
    crate::sys::tone(&mut bytes, 1.35, 0.9); // the glass's toning, done here instead of by a filter on the page
    let img = image::RgbImage::from_raw(w as u32, hh as u32, bytes)?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&img).ok()?;
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(out.into_inner())))
}

/// What's behind ULTRON's overlay, one picture at a time: the slow way, for while only the bar shows.
fn capture(app: &AppHandle) -> Option<String> {
    let _busy = BUSY.lock().ok()?;
    if paused() {
        return None;
    }
    let (h, r) = window(app)?;
    let (w, hh) = small_size(r);
    // leave ULTRON out of this one capture (it stays on screen the whole time)
    let shot = unsafe {
        let set = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| crate::log_line(&format!("live glass: exclude-from-capture {:?}", set)));
        std::thread::sleep(Duration::from_millis(34)); // two frames for the compositor to take it out
        let px = grab_small(r, w as i32, hh as i32);
        let _ = SetWindowDisplayAffinity(h, WDA_NONE);
        px?
    };
    finish(&shot, w, hh)
}

/// The live feed: each new picture of the screen, handed over by Windows as it is drawn.
mod live {
    use windows::core::Interface;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::*;

    pub struct Feed {
        ctx: ID3D11DeviceContext,
        dup: IDXGIOutputDuplication,
        stage: ID3D11Texture2D,
        part: RECT, // the part of that screen under ULTRON
        _device: ID3D11Device,
    }

    impl Feed {
        /// Follow the screen that `r` (ULTRON's window, in desktop pixels) is on.
        pub unsafe fn open(r: RECT) -> Option<Feed> {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    o += 1;
                    let Ok(desc) = output.GetDesc() else { continue };
                    let d = desc.DesktopCoordinates;
                    if r.left < d.left || r.left >= d.right || r.top < d.top || r.top >= d.bottom {
                        continue; // (another screen)
                    }
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                        return None; // (a turned screen: the slow way handles it)
                    }
                    let (mut device, mut ctx) = (None, None);
                    D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, Default::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx)).ok()?;
                    let (device, ctx): (ID3D11Device, ID3D11DeviceContext) = (device?, ctx?);
                    let dup = output.cast::<IDXGIOutput1>().ok()?.DuplicateOutput(&device).ok()?;
                    let part = RECT { left: r.left - d.left, top: r.top - d.top, right: (r.right - d.left).min(d.right - d.left), bottom: (r.bottom - d.top).min(d.bottom - d.top) };
                    let td = D3D11_TEXTURE2D_DESC {
                        Width: (part.right - part.left) as u32,
                        Height: (part.bottom - part.top) as u32,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut stage = None;
                    device.CreateTexture2D(&td, None, Some(&mut stage)).ok()?;
                    return Some(Feed { ctx, dup, stage: stage?, part, _device: device });
                }
            }
            None
        }

        /// The next picture, shrunk to w x h (BGRA). Ok(None) when nothing on screen changed in `wait_ms`;
        /// Err when the feed is gone (the screen changed its mode, or was locked) and has to be opened again.
        pub unsafe fn next(&mut self, wait_ms: u32, w: usize, h: usize) -> Result<Option<Vec<u8>>, ()> {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            if let Err(e) = self.dup.AcquireNextFrame(wait_ms, &mut info, &mut res) {
                return if e.code() == DXGI_ERROR_WAIT_TIMEOUT { Ok(None) } else { Err(()) };
            }
            let shot = self.read(&info, res.as_ref(), w, h);
            drop(res);
            let _ = self.dup.ReleaseFrame();
            Ok(shot)
        }

        unsafe fn read(&self, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<&IDXGIResource>, w: usize, h: usize) -> Option<Vec<u8>> {
            if info.LastPresentTime == 0 || info.TotalMetadataBufferSize == 0 {
                return None; // (only the pointer moved, or nothing on screen was redrawn: no need to read the picture back)
            }
            let tex: ID3D11Texture2D = res?.cast().ok()?;
            let p = self.part;
            let bx = D3D11_BOX { left: p.left as u32, top: p.top as u32, front: 0, right: p.right as u32, bottom: p.bottom as u32, back: 1 };
            self.ctx.CopySubresourceRegion(&self.stage, 0, 0, 0, 0, &tex, 0, Some(&bx));
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&self.stage, 0, D3D11_MAP_READ, 0, Some(&mut map)).ok()?;
            let out = shrink(map.pData as *const u8, map.RowPitch as usize, (p.right - p.left) as usize, (p.bottom - p.top) as usize, w, h);
            self.ctx.Unmap(&self.stage, 0);
            Some(out)
        }
    }

    /// Shrink by averaging (every other pixel each way is plenty under a blur, and four times quicker).
    unsafe fn shrink(p: *const u8, pitch: usize, sw: usize, sh: usize, w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![255u8; w * h * 4];
        for y in 0..h {
            let y0 = y * sh / h;
            let y1 = ((y + 1) * sh / h).max(y0 + 1).min(sh);
            for x in 0..w {
                let x0 = x * sw / w;
                let x1 = ((x + 1) * sw / w).max(x0 + 1).min(sw);
                let (mut b, mut g, mut r, mut n) = (0u32, 0u32, 0u32, 0u32);
                let mut yy = y0;
                while yy < y1 {
                    let row = p.add(yy * pitch);
                    let mut xx = x0;
                    while xx < x1 {
                        let q = row.add(xx * 4);
                        b += *q as u32;
                        g += *q.add(1) as u32;
                        r += *q.add(2) as u32;
                        n += 1;
                        xx += 2;
                    }
                    yy += 2;
                }
                let o = (y * w + x) * 4;
                if n > 0 {
                    out[o] = (b / n) as u8;
                    out[o + 1] = (g / n) as u8;
                    out[o + 2] = (r / n) as u8;
                }
            }
        }
        out
    }
}

pub fn start(app: AppHandle) {
    snap_watch();
    others_watch();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let mut last_log = Instant::now() - Duration::from_secs(600);
        let mut feed: Option<live::Feed> = None;
        let (mut dims, mut last, mut retry) = ((0usize, 0usize), Vec::new(), Instant::now());
        let (mut shown, mut spent, mut since, mut told) = (0u32, Duration::ZERO, Instant::now(), false);
        let (mut sent, mut sent_at) = (String::new(), Instant::now());
        loop {
            let every = EVERY.load(Ordering::SeqCst);
            let fast = every > 0 && every <= FAST && !paused();
            if !fast && feed.take().is_some() {
                LIVE.store(false, Ordering::SeqCst);
                exclude(false); // (back in everyone's screenshots)
            }
            if every == 0 || paused() {
                std::thread::sleep(Duration::from_millis(if every == 0 { 300 } else { 60 }));
                continue;
            }
            if fast {
                if feed.is_none() && Instant::now() >= retry {
                    let _busy = BUSY.lock();
                    if let Some((_, r)) = window(&app).filter(|_| !paused()) {
                        exclude(true);
                        std::thread::sleep(Duration::from_millis(40)); // (the compositor takes ULTRON out before the first picture)
                        feed = unsafe { live::Feed::open(r) };
                        if feed.is_some() {
                            LIVE.store(true, Ordering::SeqCst);
                            (dims, shown, spent, since) = (small_size(r), 0, Duration::ZERO, Instant::now());
                        } else {
                            exclude(false);
                            retry = Instant::now() + Duration::from_secs(30);
                            crate::log_line("live glass: the live feed isn't available on this screen; refreshing the slow way");
                        }
                    }
                }
                if let Some(f) = feed.as_mut() {
                    let t0 = Instant::now();
                    match unsafe { f.next(100, dims.0, dims.1) } {
                        // (a picture taken as ULTRON was being put back into captures would have ULTRON in it)
                        Ok(Some(shot)) if !paused() && shot != last => {
                            if let Some(url) = finish(&shot, dims.0, dims.1) {
                                let _ = app.emit_to("main", "backdrop", url);
                                shown += 1;
                                spent += t0.elapsed();
                            }
                            last = shot;
                        }
                        Ok(_) => {}
                        Err(()) => {
                            feed = None;
                            LIVE.store(false, Ordering::SeqCst);
                            exclude(false);
                            retry = Instant::now() + Duration::from_millis(500);
                            continue;
                        }
                    }
                    if !told && shown >= 20 {
                        told = true;
                        crate::log_line(&format!("live glass: following the screen as it's drawn ({} pictures in {:.1} s, {} ms each)", shown, since.elapsed().as_secs_f32(), spent.as_millis() / shown as u128));
                    }
                    std::thread::sleep(Duration::from_millis(every).saturating_sub(t0.elapsed()));
                    continue;
                }
            }
            let t0 = Instant::now();
            if let Some(url) = capture(&app) {
                // (nothing behind ULTRON has changed since the last look: the page already has this picture)
                if url != sent || sent_at.elapsed() > Duration::from_secs(60) {
                    let _ = app.emit_to("main", "backdrop", &url);
                    (sent, sent_at) = (url, Instant::now());
                }
            }
            let took = t0.elapsed();
            if last_log.elapsed() > Duration::from_secs(300) {
                last_log = Instant::now();
                crate::log_line(&format!("live glass: {} ms per refresh", took.as_millis()));
            }
            // (The wait is taken in short steps, so a panel opening, or a kick, is noticed at once. With only the bar
            //  showing the page asks for a look every few seconds: behind the bar there is only the wallpaper, and looking
            //  several times a second cost a tenth of a processor core, all day.)
            let until = Instant::now() + Duration::from_millis(every.max(160)).saturating_sub(took).max(Duration::from_millis(60));
            while Instant::now() < until && EVERY.load(Ordering::SeqCst) == every && !KICK.swap(false, Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blur_keeps_flat_colour() {
        let (w, h) = (20, 10);
        let mut px = vec![100f32; w * h * 3];
        px[(5 * w + 10) * 3] = 255.0;
        blur(&mut px, w, h, 3);
        assert!(px.iter().all(|v| (99.0..=110.0).contains(v)));
        let total: f32 = px.iter().step_by(3).sum();
        assert!((total - (100.0 * (w * h) as f32 + 155.0)).abs() < 40.0);
    }
}
