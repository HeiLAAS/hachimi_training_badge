// Test build: transparent always-on-top overlay window owned by the plugin.
// Cycles 速度 -> 耐力 -> 力量 -> 根性 -> 智力 to verify the badge image and
// the five-slot ordering. Created inside the game process, hidden whenever
// the game window is not foreground (so Alt-Tab switching stays clean).

use crate::{log_i, wide};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

pub struct Config {
    pub bmp_path: Vec<u16>,
    pub interval_ms: u32,
    pub offset_x: i32,
    pub offset_y: i32,
}

static CFG: OnceLock<Config> = OnceLock::new();
static INDEX: AtomicUsize = AtomicUsize::new(0);
static LAST_SHOW: AtomicUsize = AtomicUsize::new(1);
const LABELS: [&str; 5] = ["速度", "耐力", "力量", "根性", "智力"];

const W: i32 = 270;
const H: i32 = 150;
const BADGE: i32 = 128;

#[repr(C)]
struct Rect { left: i32, top: i32, right: i32, bottom: i32 }
#[repr(C)]
struct WndClassW {
    style: u32,
    wnd_proc: usize,
    cls_extra: i32,
    wnd_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu: *const u16,
    class_name: *const u16,
}
#[repr(C)]
struct Msg {
    hwnd: *mut c_void, message: u32, wparam: usize, lparam: isize,
    time: u32, pt_x: i32, pt_y: i32, private: u32,
}
#[repr(C)]
struct Paint {
    hdc: *mut c_void, erase: i32, rc: Rect,
    restore: i32, inc: i32, reserved: [u8; 32],
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(w: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32, cls: *const u16, name: *const u16, style: u32,
        x: i32, y: i32, w: i32, h: i32, parent: *mut c_void,
        menu: *mut c_void, inst: *mut c_void, param: *mut c_void,
    ) -> *mut c_void;
    fn DefWindowProcW(h: *mut c_void, m: u32, w: usize, l: isize) -> isize;
    fn GetMessageW(m: *mut Msg, h: *mut c_void, min: u32, max: u32) -> i32;
    fn TranslateMessage(m: *const Msg) -> i32;
    fn DispatchMessageW(m: *const Msg) -> isize;
    fn PostQuitMessage(c: i32);
    fn SetTimer(h: *mut c_void, id: usize, ms: u32, cb: *mut c_void) -> usize;
    fn InvalidateRect(h: *mut c_void, r: *const Rect, erase: i32) -> i32;
    fn BeginPaint(h: *mut c_void, p: *mut Paint) -> *mut c_void;
    fn EndPaint(h: *mut c_void, p: *const Paint) -> i32;
    fn SetLayeredWindowAttributes(h: *mut c_void, key: u32, alpha: u8, flags: u32) -> i32;
    fn GetForegroundWindow() -> *mut c_void;
    fn ShowWindow(h: *mut c_void, cmd: i32) -> i32;
    fn SetWindowPos(h: *mut c_void, after: *mut c_void, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn GetSystemMetrics(i: i32) -> i32;
    fn GetWindowThreadProcessId(h: *mut c_void, pid: *mut u32) -> u32;
    fn LoadImageW(inst: *mut c_void, name: *const u16, typ: u32, cx: i32, cy: i32, flags: u32) -> *mut c_void;
    fn FillRect(hdc: *mut c_void, r: *const Rect, brush: *mut c_void) -> i32;
    fn DrawTextW(hdc: *mut c_void, text: *const u16, len: i32, r: *mut Rect, fmt: u32) -> i32;
}
#[link(name = "gdi32")]
extern "system" {
    fn CreateSolidBrush(c: u32) -> *mut c_void;
    fn DeleteObject(o: *mut c_void) -> i32;
    fn CreateCompatibleDC(hdc: *mut c_void) -> *mut c_void;
    fn DeleteDC(hdc: *mut c_void) -> i32;
    fn SelectObject(hdc: *mut c_void, o: *mut c_void) -> *mut c_void;
    fn BitBlt(dst: *mut c_void, x: i32, y: i32, w: i32, h: i32, src: *mut c_void, sx: i32, sy: i32, rop: u32) -> i32;
    fn CreateFontW(
        h: i32, w: i32, esc: i32, orient: i32, weight: i32,
        italic: u32, underline: u32, strike: u32, charset: u32,
        outp: u32, clip: u32, quality: u32, pitch: u32, face: *const u16,
    ) -> *mut c_void;
    fn SetBkMode(hdc: *mut c_void, m: i32) -> i32;
    fn SetTextColor(hdc: *mut c_void, c: u32) -> u32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetModuleHandleW(n: *const u16) -> *mut c_void;
}

const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const LWA_COLORKEY: u32 = 0x1;
const SWP_NOACTIVATE: u32 = 0x10;
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const SRCCOPY: u32 = 0x00CC_0020;
const LR_LOADFROMFILE: u32 = 0x10;
const TRANSPARENT: i32 = 1;
const DT_CENTER: u32 = 0x1;
const DT_VCENTER: u32 = 0x4;
const DT_SINGLELINE: u32 = 0x20;
const WM_PAINT: u32 = 0x000F;
const WM_TIMER: u32 = 0x0113;
const WM_DESTROY: u32 = 0x0002;
const MAGENTA: u32 = 0x00FF_00FF; // COLORREF 0x00BBGGRR
const GOLD: u32 = 0x0000_D7FF;

unsafe fn paint(h: *mut c_void) {
    let mut ps: Paint = std::mem::zeroed();
    let hdc = BeginPaint(h, &mut ps);
    if hdc.is_null() {
        return;
    }
    let cfg = CFG.get().unwrap();

    let full = Rect { left: 0, top: 0, right: W, bottom: H };
    let brush = CreateSolidBrush(MAGENTA);
    FillRect(hdc, &full, brush);
    DeleteObject(brush);

    let hbmp = LoadImageW(ptr::null_mut(), cfg.bmp_path.as_ptr(), 0, 0, 0, LR_LOADFROMFILE);
    if !hbmp.is_null() {
        let mem = CreateCompatibleDC(hdc);
        let old = SelectObject(mem, hbmp);
        BitBlt(hdc, 8, 8, BADGE, BADGE, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteDC(mem);
        DeleteObject(hbmp);
    } else {
        log_i("[DEMO] LoadImageW(badge bmp) failed");
    }

    let face = wide("Microsoft YaHei");
    let font = CreateFontW(-26, 0, 0, 0, 700, 0, 0, 0, 134, 0, 0, 4, 0, face.as_ptr());
    let old_font = SelectObject(hdc, font);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, GOLD);

    let i = INDEX.load(Ordering::Relaxed) % 5;
    let text = wide(&format!("最优 {}", LABELS[i]));
    let mut tr = Rect { left: 142, top: 30, right: W - 6, bottom: 82 };
    DrawTextW(hdc, text.as_ptr(), -1, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    let sub = wide(&format!("{}/5  {}", i + 1, LABELS[i]));
    let mut sr = Rect { left: 142, top: 84, right: W - 6, bottom: 122 };
    DrawTextW(hdc, sub.as_ptr(), -1, &mut sr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    SelectObject(hdc, old_font);
    DeleteObject(font);
    EndPaint(h, &ps);
}

unsafe extern "system" fn wndproc(h: *mut c_void, m: u32, w: usize, l: isize) -> isize {
    match m {
        WM_PAINT => {
            paint(h);
            0
        }
        WM_TIMER => {
            INDEX.fetch_add(1, Ordering::Relaxed);
            let fg = GetForegroundWindow();
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(fg, &mut pid);
            let show = pid == GetCurrentProcessId();
            let prev = LAST_SHOW.swap(show as usize, Ordering::Relaxed);
            if prev != show as usize {
                ShowWindow(h, if show { SW_SHOW } else { SW_HIDE });
            }
            if show {
                InvalidateRect(h, ptr::null(), 0);
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(h, m, w, l),
    }
}

unsafe fn window_thread() {
    let cls = wide("TrainingBadgeDemo");
    let wc = WndClassW {
        style: 0,
        wnd_proc: wndproc as *const () as usize,
        cls_extra: 0,
        wnd_extra: 0,
        instance: GetModuleHandleW(ptr::null()),
        icon: ptr::null_mut(),
        cursor: ptr::null_mut(),
        background: ptr::null_mut(),
        menu: ptr::null(),
        class_name: cls.as_ptr(),
    };
    if RegisterClassW(&wc) == 0 {
        log_i("[DEMO] RegisterClassW failed");
        return;
    }

    let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
    let hwnd = CreateWindowExW(
        ex, cls.as_ptr(), ptr::null(), WS_POPUP,
        0, 0, W, H,
        ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut(),
    );
    if hwnd.is_null() {
        log_i("[DEMO] CreateWindowExW failed");
        return;
    }
    SetLayeredWindowAttributes(hwnd, MAGENTA, 255, LWA_COLORKEY);

    let cfg = CFG.get().unwrap();
    let sw = GetSystemMetrics(0);
    let sh = GetSystemMetrics(1);
    let x = sw - W - cfg.offset_x;
    let y = sh - H - cfg.offset_y;
    SetWindowPos(hwnd, -1isize as *mut c_void, x, y, W, H, SWP_NOACTIVATE);
    ShowWindow(hwnd, SW_SHOW);
    SetTimer(hwnd, 1, cfg.interval_ms, ptr::null_mut());
    log_i("[DEMO] overlay created, cycling 速度/耐力/力量/根性/智力");

    let mut msg: Msg = std::mem::zeroed();
    while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

pub fn run(cfg: Config) {
    let _ = CFG.set(cfg);
    std::thread::spawn(|| unsafe { window_thread() });
    log_i("[DEMO] window thread spawned");
}
