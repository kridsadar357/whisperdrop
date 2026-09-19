//! Edge drop zone (Windows).
//!
//! One slim always-on-top strip per monitor, docked to the configured screen
//! side, mirroring the macOS widget in `src/edge.html`:
//!
//! * idle — a glowing pill on the edge; the whole edge height accepts drops
//! * drag-over — the strip widens into a "Drop to send" glass card
//! * drop — a glass picker lists LAN peers and the tunnel group, then shows
//!   per-file progress and closes itself
//!
//! Everything is rendered in software (`glass.rs`) into per-pixel-alpha
//! layered windows; monitors are re-enumerated on `WM_DISPLAYCHANGE`.

#[cfg(windows)]
mod imp {
    use crate::glass::{self, gdi::{BOOL, HWND, POINT, RECT}, Canvas, Rgba, TextRun};
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    // ------------------------------------------------------------ Win32 FFI
    type UINT = u32;
    type HMENU = isize;
    type WNDPROC = Option<unsafe extern "system" fn(HWND, UINT, usize, isize) -> isize>;

    #[repr(C)]
    struct MSG {
        hwnd: HWND,
        message: UINT,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt: POINT,
    }
    #[repr(C)]
    struct WNDCLASSW {
        style: u32,
        proc_: WNDPROC,
        extra_cls: i32,
        extra_win: i32,
        instance: isize,
        icon: isize,
        cursor: isize,
        background: isize,
        menu: *const u16,
        class_name: *const u16,
    }
    #[repr(C)]
    struct MONITORINFO {
        cb: u32,
        rc_monitor: RECT,
        rc_work: RECT,
        flags: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassW(class: *const WNDCLASSW) -> u16;
        fn CreateWindowExW(ex: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32,
            parent: HWND, menu: isize, inst: isize, param: *mut std::ffi::c_void) -> HWND;
        fn DefWindowProcW(hwnd: HWND, msg: UINT, w: usize, l: isize) -> isize;
        fn DestroyWindow(hwnd: HWND) -> BOOL;
        fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
        fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: UINT, max: UINT) -> i32;
        fn TranslateMessage(msg: *const MSG) -> BOOL;
        fn DispatchMessageW(msg: *const MSG) -> isize;
        fn SetWindowPos(hwnd: HWND, after: HWND, x: i32, y: i32, w: i32, h: i32, flags: UINT) -> BOOL;
        fn SetTimer(hwnd: HWND, id: usize, ms: u32, proc_: isize) -> usize;
        fn KillTimer(hwnd: HWND, id: usize) -> BOOL;
        fn GetCursorPos(pt: *mut POINT) -> BOOL;
        fn GetAsyncKeyState(key: i32) -> i16;
        fn EnumDisplayMonitors(dc: isize, clip: *const RECT, cb: unsafe extern "system" fn(isize, isize, *mut RECT, isize) -> BOOL, data: isize) -> BOOL;
        fn GetMonitorInfoW(monitor: isize, info: *mut MONITORINFO) -> BOOL;
        fn GetDC(hwnd: HWND) -> isize;
        fn ReleaseDC(hwnd: HWND, dc: isize) -> i32;
        fn SetProcessDPIAware() -> BOOL;
        fn CreatePopupMenu() -> HMENU;
        fn AppendMenuW(menu: HMENU, flags: u32, id: usize, text: *const u16) -> BOOL;
        fn TrackPopupMenu(menu: HMENU, flags: u32, x: i32, y: i32, reserved: i32, hwnd: HWND, rect: *const RECT) -> BOOL;
        fn DestroyMenu(menu: HMENU) -> BOOL;
        fn SetForegroundWindow(hwnd: HWND) -> BOOL;
        fn PostMessageW(hwnd: HWND, msg: UINT, w: usize, l: isize) -> BOOL;
        fn GetModuleHandleW(name: *const u16) -> isize;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn GetDeviceCaps(dc: isize, index: i32) -> i32;
    }
    #[link(name = "shell32")]
    extern "system" {
        fn DragAcceptFiles(hwnd: HWND, accept: BOOL);
        fn DragQueryFileW(hdrop: isize, index: u32, buf: *mut u16, len: u32) -> u32;
        fn DragFinish(hdrop: isize);
    }

    const WS_POPUP: u32 = 0x8000_0000;
    const WS_EX_TOPMOST: u32 = 0x8;
    const WS_EX_TOOLWINDOW: u32 = 0x80;
    const WS_EX_LAYERED: u32 = 0x8_0000;
    const WS_EX_NOACTIVATE: u32 = 0x800_0000;
    const SW_SHOWNOACTIVATE: i32 = 4;
    const SWP_NOACTIVATE: u32 = 0x10;
    const HWND_TOPMOST: HWND = -1;
    const WM_DESTROY: UINT = 0x2;
    const WM_TIMER: UINT = 0x113;
    const WM_MOUSEMOVE: UINT = 0x200;
    const WM_LBUTTONDOWN: UINT = 0x201;
    const WM_LBUTTONUP: UINT = 0x202;
    const WM_RBUTTONUP: UINT = 0x205;
    const WM_DROPFILES: UINT = 0x233;
    const WM_DISPLAYCHANGE: UINT = 0x7E;
    const WM_APP_OPEN_PICKER: UINT = 0x8001;
    const WM_APP_RESYNC: UINT = 0x8002;
    const TPM_RETURNCMD: u32 = 0x100;
    const MF_SEPARATOR: u32 = 0x800;
    const VK_LBUTTON: i32 = 0x01;
    const TICK_MS: u32 = 33;

    // --------------------------------------------------- shared geometry
    // Logical px — identical to edge.html / edge.rs; scaled by the DPI.
    const STRIP_W: f64 = 14.0;
    const WIDE_W: f64 = 168.0;
    const PANEL_W: f64 = 340.0;
    const PANEL_H: f64 = 500.0;

    // ------------------------------------------------------------- state
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum StripMode {
        Idle,
        Wide,
    }

    #[derive(Clone)]
    struct Strip {
        hwnd: HWND,
        work: RECT,
        mode: StripMode,
        /// 0..1 hover emphasis, eased each tick
        hover: f64,
        /// last rendered (mode, hover*100) — skip identical frames
        last: (StripMode, i32),
        pressed_inside: bool,
    }

    #[derive(Clone)]
    enum Row {
        Lan { name: String, ip: String, port: u16 },
        Tunnel { group: String },
        Retry,
    }

    #[derive(Clone, PartialEq)]
    enum XferState {
        Queued,
        Sending,
        Done,
        Failed(String),
    }

    struct Xfer {
        name: String,
        sent: Arc<AtomicU64>,
        total: Arc<AtomicU64>,
        indeterminate: bool,
        state: XferState,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Phase {
        Choose,
        Sending,
    }

    struct Picker {
        hwnd: HWND,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        opened_at: u64,
        phase: Phase,
        files: Vec<String>,
        rows: Vec<Row>,
        hover: Option<usize>,
        hover_close: bool,
        hover_refresh: bool,
        title: String,
        subtitle: String,
        xfers: Arc<Mutex<Vec<Xfer>>>,
        done_at: Option<u64>,
        all_ok: bool,
    }

    static SIDE: Mutex<String> = Mutex::new(String::new());
    static RT_HANDLE: Mutex<Option<tokio::runtime::Handle>> = Mutex::new(None);
    pub static STRIP_PORT: AtomicU16 = AtomicU16::new(51731);
    static SCALE_MILLI: AtomicU64 = AtomicU64::new(1000);
    static STRIPS: Mutex<Vec<Strip>> = Mutex::new(Vec::new());
    static PICKER: Mutex<Option<Picker>> = Mutex::new(None);
    /// Where the left button went down (screen px), while it is held.
    static PRESS_AT: Mutex<Option<POINT>> = Mutex::new(None);
    static ANY_HWND: AtomicU64 = AtomicU64::new(0);
    static CLASSES_READY: AtomicBool = AtomicBool::new(false);
    static RESYNC_PENDING: AtomicBool = AtomicBool::new(false);

    fn scale() -> f64 {
        SCALE_MILLI.load(Ordering::Relaxed) as f64 / 1000.0
    }
    fn px(v: f64) -> i32 {
        (v * scale()).round() as i32
    }
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    fn side_left() -> bool {
        SIDE.lock().unwrap().as_str() == "left"
    }
    fn contains(r: &RECT, p: POINT) -> bool {
        p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    }
    fn lbutton_down() -> bool {
        unsafe { GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000 != 0 }
    }
    fn cursor() -> POINT {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) };
        p
    }

    /// Screen rect of a strip window in `mode`.
    fn strip_rect(work: &RECT, mode: StripMode) -> RECT {
        let w = px(if mode == StripMode::Wide { WIDE_W } else { STRIP_W });
        if side_left() {
            RECT { left: work.left, top: work.top, right: work.left + w, bottom: work.bottom }
        } else {
            RECT { left: work.right - w, top: work.top, right: work.right, bottom: work.bottom }
        }
    }

    // ------------------------------------------------------------ monitors
    unsafe extern "system" fn monitor_cb(monitor: isize, _dc: isize, _rc: *mut RECT, data: isize) -> BOOL {
        let out = &mut *(data as *mut Vec<RECT>);
        let mut info = MONITORINFO { cb: std::mem::size_of::<MONITORINFO>() as u32, rc_monitor: RECT::default(), rc_work: RECT::default(), flags: 0 };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            out.push(info.rc_work);
        }
        1
    }

    fn monitors() -> Vec<RECT> {
        let mut out: Vec<RECT> = Vec::new();
        unsafe {
            EnumDisplayMonitors(0, std::ptr::null(), monitor_cb, &mut out as *mut _ as isize);
        }
        if out.is_empty() {
            out.push(RECT { left: 0, top: 0, right: 1920, bottom: 1080 });
        }
        out
    }

    // --------------------------------------------------------- strip window
    unsafe fn create_strip(work: RECT) -> HWND {
        let r = strip_rect(&work, StripMode::Idle);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            glass::wide("WhisperDropStrip").as_ptr(),
            glass::wide("WhisperDrop").as_ptr(),
            WS_POPUP,
            r.left, r.top, r.right - r.left, r.bottom - r.top,
            0, 0, GetModuleHandleW(std::ptr::null()), std::ptr::null_mut(),
        );
        DragAcceptFiles(hwnd, 1);
        SetTimer(hwnd, 1, TICK_MS, 0);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        hwnd
    }

    /// (Re)create one strip per monitor. Runs on the UI thread.
    unsafe fn sync_strips() {
        let old: Vec<HWND> = STRIPS.lock().unwrap().drain(..).map(|s| s.hwnd).collect();
        for h in old {
            KillTimer(h, 1);
            DestroyWindow(h);
        }
        let mut strips = Vec::new();
        for work in monitors() {
            let hwnd = create_strip(work);
            strips.push(Strip { hwnd, work, mode: StripMode::Idle, hover: 0.0, last: (StripMode::Idle, -1), pressed_inside: false });
        }
        println!("[dropzone] {} edge strip(s) on the {}", strips.len(), SIDE.lock().unwrap());
        for s in &strips {
            render_strip(s, 0.0);
        }
        ANY_HWND.store(strips.first().map(|s| s.hwnd as u64).unwrap_or(0), Ordering::Relaxed);
        *STRIPS.lock().unwrap() = strips;
    }

    unsafe fn apply_mode(hwnd: HWND, work: &RECT, mode: StripMode) {
        let r = strip_rect(work, mode);
        SetWindowPos(hwnd, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE);
    }

    /// One animation/interaction tick for a strip.
    unsafe fn strip_tick(hwnd: HWND) {
        let cur = cursor();
        let down = lbutton_down();
        // global press tracking (any strip's timer may record it)
        {
            let mut p = PRESS_AT.lock().unwrap();
            if down && p.is_none() {
                *p = Some(cur);
            } else if !down {
                *p = None;
            }
        }
        let press_at = *PRESS_AT.lock().unwrap();

        let mut action: Option<(RECT, StripMode)> = None;
        let mut frame: Option<(Strip, f64)> = None;
        {
            let mut strips = STRIPS.lock().unwrap();
            let Some(s) = strips.iter_mut().find(|s| s.hwnd == hwnd) else { return };
            let idle_rect = strip_rect(&s.work, StripMode::Idle);
            let cur_rect = strip_rect(&s.work, s.mode);
            let inside = contains(&cur_rect, cur);
            let picker_open = PICKER.lock().unwrap().is_some();

            // a drag = button held since a press that started outside this strip
            let dragging_in = down && inside && press_at.map(|p| !contains(&idle_rect, p)).unwrap_or(false);
            let next = match s.mode {
                StripMode::Idle if dragging_in && !picker_open => StripMode::Wide,
                StripMode::Wide if !down || !inside => StripMode::Idle,
                m => m,
            };
            if next != s.mode {
                s.mode = next;
                action = Some((s.work, next));
            }
            let target = if s.mode == StripMode::Idle && inside && !down { 1.0 } else { 0.0 };
            s.hover += (target - s.hover) * 0.22;
            if (s.hover - target).abs() < 0.01 {
                s.hover = target;
            }
            let key = (s.mode, (s.hover * 100.0) as i32);
            if key != s.last || s.mode == StripMode::Wide {
                s.last = key;
                frame = Some((s.clone(), now_ms() as f64 / 1000.0));
            }
        }
        if let Some((work, mode)) = action {
            apply_mode(hwnd, &work, mode);
        }
        if let Some((s, t)) = frame {
            render_strip(&s, t);
        }
    }

    /// Draw the idle pill or the drag-over card for a strip.
    unsafe fn render_strip(s: &Strip, t: f64) {
        let r = strip_rect(&s.work, s.mode);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut c = Canvas::new(w, h);
        let left = side_left();
        let sc = scale();
        // Idle: only the outer 6px column (+ the pill) takes input so clicks
        // on a maximized window's scrollbar still get through; the drag card
        // takes the whole width.
        match s.mode {
            StripMode::Idle => {
                let hit = px(6.0);
                c.hit_area(if left { 0 } else { w - hit }, 0, hit, h);
            }
            StripMode::Wide => c.hit_area(0, 0, w, h),
        }

        match s.mode {
            StripMode::Idle => {
                let pw = 5.0 * sc + 2.0 * sc * s.hover;
                let ph = 128.0 * sc + 32.0 * sc * s.hover;
                let x = if left { 4.0 * sc } else { w as f64 - 4.0 * sc - pw };
                let y = (h as f64 - ph) / 2.0;
                let glow = glass::with_alpha(glass::ACCENT_A, (140.0 + 60.0 * s.hover) as u8);
                c.glow(x, y, pw, ph, pw / 2.0, 10.0 * sc + 8.0 * sc * s.hover, glow, 0.8);
                c.rrect(x, y, pw, ph, pw / 2.0, glass::ACCENT_A, glass::ACCENT_B, 0.85 + 0.15 * s.hover);
            }
            StripMode::Wide => {
                // edge glow column (edge.html #edgeGlow)
                for xx in 0..w {
                    let tt = if left { 1.0 - xx as f64 / w as f64 } else { xx as f64 / w as f64 };
                    let a = if tt < 0.7 { tt / 0.7 * 0.14 } else { 0.14 + (tt - 0.7) / 0.3 * 0.16 };
                    let col = glass::lerp(glass::ACCENT_A, glass::ACCENT_B, tt);
                    let col = glass::with_alpha(col, (a * 255.0) as u8);
                    for yy in 0..h {
                        c.blend(xx, yy, col);
                    }
                }
                // "Drop to send" card
                let (cw, ch) = (140.0 * sc, 184.0 * sc);
                let cx = if left { 12.0 * sc } else { w as f64 - 12.0 * sc - cw };
                let cy = (h as f64 - ch) / 2.0;
                c.glow(cx, cy, cw, ch, glass::RADIUS * sc, 18.0 * sc, (0, 0, 0, 120), 1.0);
                c.rrect(cx, cy, cw, ch, glass::RADIUS * sc, glass::GLASS_SOLID, glass::GLASS_SOLID, 1.0);
                c.rrect_stroke(cx, cy, cw, ch, glass::RADIUS * sc, glass::LINE, 1.0);
                // dashed ring + bobbing arrow
                let (rx, ry) = (cx + cw / 2.0, cy + 62.0 * sc);
                let rad = 32.0 * sc;
                c.ring(rx, ry, rad, 2.0 * sc, (255, 255, 255, 90), Some((9.0 * sc, 15.0 * sc, (t * 12.0 * sc) % (15.0 * sc))));
                let bob = (t * std::f64::consts::TAU / 1.1).sin() * 3.0 * sc;
                let (ax, ay) = (rx, ry + bob);
                let white = (255, 255, 255, 255);
                c.line(ax, ay - 12.0 * sc, ax, ay + 8.0 * sc, 2.6 * sc, white);
                c.line(ax - 7.0 * sc, ay + 1.0 * sc, ax, ay + 8.0 * sc, 2.6 * sc, white);
                c.line(ax + 7.0 * sc, ay + 1.0 * sc, ax, ay + 8.0 * sc, 2.6 * sc, white);
                c.line(ax - 8.0 * sc, ay + 15.0 * sc, ax + 8.0 * sc, ay + 15.0 * sc, 2.4 * sc, white);
                let (tx, tw) = (cx as i32, cw as i32);
                c.text(&TextRun::new("Drop to send", tx, (cy + 110.0 * sc) as i32, tw, px(20.0), px(14.0)).bold(700).align(glass::ALIGN_CENTER));
                c.text(&TextRun::new("to a device nearby", tx, (cy + 136.0 * sc) as i32, tw, px(15.0), px(11.0)).color(glass::MUTED).align(glass::ALIGN_CENTER));
                c.text(&TextRun::new("or in your group", tx, (cy + 151.0 * sc) as i32, tw, px(15.0), px(11.0)).color(glass::MUTED).align(glass::ALIGN_CENTER));
            }
        }
        glass::gdi::present(s.hwnd, &c, r.left, r.top);
    }

    unsafe extern "system" fn strip_proc(hwnd: HWND, msg: UINT, w: usize, l: isize) -> isize {
        match msg {
            WM_TIMER => {
                strip_tick(hwnd);
                0
            }
            WM_LBUTTONDOWN => {
                if let Some(s) = STRIPS.lock().unwrap().iter_mut().find(|s| s.hwnd == hwnd) {
                    s.pressed_inside = true;
                }
                0
            }
            WM_LBUTTONUP => {
                let clicked = {
                    let mut strips = STRIPS.lock().unwrap();
                    let s = strips.iter_mut().find(|s| s.hwnd == hwnd);
                    s.map(|s| std::mem::replace(&mut s.pressed_inside, false)).unwrap_or(false)
                };
                if clicked {
                    open_picker(hwnd, Vec::new());
                }
                0
            }
            WM_RBUTTONUP => {
                context_menu(hwnd);
                0
            }
            WM_DROPFILES => {
                let hdrop = w as isize;
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
                let mut paths = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                    let mut buf = vec![0u16; (len + 1) as usize];
                    DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                    paths.push(String::from_utf16_lossy(&buf[..len as usize]));
                }
                DragFinish(hdrop);
                // collapse the drag card right away
                let work = {
                    let mut strips = STRIPS.lock().unwrap();
                    strips.iter_mut().find(|s| s.hwnd == hwnd).map(|s| {
                        s.mode = StripMode::Idle;
                        s.last = (StripMode::Idle, -1);
                        s.work
                    })
                };
                if let Some(work) = work {
                    apply_mode(hwnd, &work, StripMode::Idle);
                }
                if paths.is_empty() {
                    println!("[dropzone] drop ignored — no file paths received");
                } else {
                    open_picker(hwnd, paths);
                }
                0
            }
            WM_DISPLAYCHANGE => {
                // every strip receives this; rebuild once
                if !RESYNC_PENDING.swap(true, Ordering::Relaxed) {
                    PostMessageW(hwnd, WM_APP_RESYNC, 0, 0);
                }
                0
            }
            WM_APP_RESYNC => {
                RESYNC_PENDING.store(false, Ordering::Relaxed);
                close_picker();
                sync_strips();
                0
            }
            WM_APP_OPEN_PICKER => {
                open_picker(hwnd, Vec::new());
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    unsafe fn context_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        let left = side_left();
        AppendMenuW(menu, 0, 1, glass::wide("Show devices").as_ptr());
        AppendMenuW(menu, 0, 2, glass::wide(if left { "Move to right edge" } else { "Move to left edge" }).as_ptr());
        AppendMenuW(menu, 0, 3, glass::wide("Open received files").as_ptr());
        AppendMenuW(menu, 0, 4, glass::wide("Preferences…").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, 0, 5, glass::wide("Quit WhisperDrop").as_ptr());
        let pt = cursor();
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD, pt.x, pt.y, 0, hwnd, std::ptr::null());
        DestroyMenu(menu);
        match chosen {
            1 => {
                PostMessageW(hwnd, WM_APP_OPEN_PICKER, 0, 0);
            }
            2 => crate::set_position(if left { "right" } else { "left" }),
            3 => {
                let dir = crate::receive_dir();
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
            4 => {
                let port = STRIP_PORT.load(Ordering::Relaxed);
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "start", "", &format!("http://127.0.0.1:{port}/wizard")])
                    .spawn();
            }
            5 => std::process::exit(0),
            _ => {}
        }
    }

    // -------------------------------------------------------------- picker
    fn build_rows() -> Vec<Row> {
        let mut rows: Vec<Row> = crate::mdns::list()
            .into_iter()
            .map(|p| Row::Lan { name: p.name, ip: p.ip, port: p.port })
            .collect();
        let group = crate::tunnel::current_group();
        if crate::relay_url().is_some() && !group.is_empty() {
            rows.push(Row::Tunnel { group });
        }
        if rows.is_empty() {
            rows.push(Row::Retry);
        }
        rows
    }

    fn basename(p: &str) -> String {
        p.rsplit(['\\', '/']).next().unwrap_or(p).to_string()
    }

    /// Open (or reset) the picker on the monitor of `strip_hwnd`.
    unsafe fn open_picker(strip_hwnd: HWND, files: Vec<String>) {
        let work = STRIPS.lock().unwrap().iter().find(|s| s.hwnd == strip_hwnd).map(|s| s.work);
        let Some(work) = work else { return };
        let (w, h) = (px(PANEL_W), px(PANEL_H).min(work.bottom - work.top));
        let x = if side_left() { work.left } else { work.right - w };
        let y = work.top + (work.bottom - work.top - h) / 2;

        let existing = PICKER.lock().unwrap().as_ref().map(|p| p.hwnd);
        let hwnd = match existing {
            Some(existing) => {
                SetWindowPos(existing, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
                existing
            }
            None => {
                let created = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                    glass::wide("WhisperDropPicker").as_ptr(),
                    glass::wide("WhisperDrop").as_ptr(),
                    WS_POPUP, x, y, w, h, 0, 0, GetModuleHandleW(std::ptr::null()), std::ptr::null_mut(),
                );
                SetTimer(created, 2, TICK_MS, 0);
                created
            }
        };
        let subtitle = match files.len() {
            0 => "Drop a file on the edge to send".to_string(),
            1 => format!("\u{201c}{}\u{201d}", basename(&files[0])),
            n => format!("{n} files"),
        };
        let picker = Picker {
            hwnd, x, y, w, h,
            opened_at: now_ms(),
            phase: Phase::Choose,
            files,
            rows: build_rows(),
            hover: None,
            hover_close: false,
            hover_refresh: false,
            title: "Send to".into(),
            subtitle,
            xfers: Arc::new(Mutex::new(Vec::new())),
            done_at: None,
            all_ok: true,
        };
        render_picker(&picker, now_ms() as f64 / 1000.0);
        *PICKER.lock().unwrap() = Some(picker);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, 0x1 | 0x2 | SWP_NOACTIVATE);
    }

    unsafe fn close_picker() {
        let hwnd = PICKER.lock().unwrap().take().map(|p| p.hwnd);
        if let Some(h) = hwnd {
            KillTimer(h, 2);
            DestroyWindow(h);
        }
    }

    // picker layout (logical px)
    const PAD: f64 = 16.0;
    const HEAD_H: f64 = 40.0;
    const ROW_H: f64 = 56.0;
    const ROW_GAP: f64 = 8.0;
    const XFER_H: f64 = 66.0;
    const FOOT_H: f64 = 36.0;

    fn list_top() -> i32 {
        px(PAD + HEAD_H + 12.0)
    }

    /// Hit-test a picker-local point: Some(row index) / close / refresh.
    fn picker_hit(p: &Picker, lx: i32, ly: i32) -> (Option<usize>, bool, bool) {
        let close = lx >= p.w - px(PAD + 24.0) && lx < p.w - px(PAD) && ly >= px(PAD) && ly < px(PAD + 24.0);
        let refresh = p.phase == Phase::Choose && lx >= px(PAD) && lx < p.w - px(PAD) && ly >= p.h - px(14.0 + FOOT_H) && ly < p.h - px(14.0);
        let mut row = None;
        if p.phase == Phase::Choose && lx >= px(PAD) && lx < p.w - px(PAD) {
            let rel = ly - list_top();
            if rel >= 0 {
                let pitch = px(ROW_H + ROW_GAP);
                let i = (rel / pitch) as usize;
                if rel % pitch < px(ROW_H) && i < p.rows.len() {
                    row = Some(i);
                }
            }
        }
        (row, close, refresh)
    }

    unsafe fn picker_tick(hwnd: HWND) {
        let cur = cursor();
        let down = lbutton_down();
        let mut close = false;
        let mut frame = false;
        {
            let mut guard = PICKER.lock().unwrap();
            let Some(p) = guard.as_mut() else { return };
            if p.hwnd != hwnd {
                return;
            }
            let inside = cur.x >= p.x && cur.x < p.x + p.w && cur.y >= p.y && cur.y < p.y + p.h;
            // click-away (after a short grace so the drop's own mouse-up is ignored)
            if down && !inside && now_ms() > p.opened_at + 400 && p.phase == Phase::Choose {
                close = true;
            }
            if p.phase == Phase::Sending {
                let xf = p.xfers.lock().unwrap();
                let finished = !xf.is_empty() && xf.iter().all(|x| matches!(x.state, XferState::Done | XferState::Failed(_)));
                if finished && p.done_at.is_none() {
                    p.done_at = Some(now_ms());
                    p.all_ok = xf.iter().all(|x| x.state == XferState::Done);
                    p.title = if p.all_ok { "Sent".into() } else { "Finished with errors".into() };
                }
                drop(xf);
                if let Some(d) = p.done_at {
                    if now_ms() > d + if p.all_ok { 2200 } else { 6000 } {
                        close = true;
                    }
                }
                frame = true; // bars animate
            }
        }
        if close {
            close_picker();
            return;
        }
        if frame {
            let guard = PICKER.lock().unwrap();
            if let Some(p) = guard.as_ref() {
                render_picker(p, now_ms() as f64 / 1000.0);
            }
        }
    }

    unsafe fn picker_mouse(hwnd: HWND, lx: i32, ly: i32, click: bool) {
        let mut chosen: Option<Row> = None;
        let mut do_close = false;
        let mut do_refresh = false;
        {
            let mut guard = PICKER.lock().unwrap();
            let Some(p) = guard.as_mut() else { return };
            if p.hwnd != hwnd {
                return;
            }
            let (row, close, refresh) = picker_hit(p, lx, ly);
            let changed = row != p.hover || close != p.hover_close || refresh != p.hover_refresh;
            p.hover = row;
            p.hover_close = close;
            p.hover_refresh = refresh;
            if click {
                if close {
                    do_close = true;
                } else if refresh {
                    do_refresh = true;
                } else if let Some(i) = row {
                    chosen = p.rows.get(i).cloned();
                }
            } else if changed {
                render_picker(p, now_ms() as f64 / 1000.0);
            }
        }
        if do_close {
            close_picker();
            return;
        }
        if do_refresh {
            let mut guard = PICKER.lock().unwrap();
            if let Some(p) = guard.as_mut() {
                p.rows = build_rows();
                render_picker(p, now_ms() as f64 / 1000.0);
            }
            return;
        }
        match chosen {
            Some(Row::Retry) => {
                let mut guard = PICKER.lock().unwrap();
                if let Some(p) = guard.as_mut() {
                    p.rows = build_rows();
                    render_picker(p, now_ms() as f64 / 1000.0);
                }
            }
            Some(row) => start_transfer(row),
            None => {}
        }
    }

    /// Queue every pending file to `row` and switch the picker to progress.
    fn start_transfer(row: Row) {
        let Some(handle) = RT_HANDLE.lock().unwrap().clone() else { return };
        let (files, xfers) = {
            let mut guard = PICKER.lock().unwrap();
            let Some(p) = guard.as_mut() else { return };
            if p.files.is_empty() {
                return; // "peek" mode — nothing to send
            }
            let (title, subtitle, indeterminate) = match &row {
                Row::Lan { name, ip, .. } => (format!("Sending to {name}"), ip.clone(), false),
                Row::Tunnel { group } => (format!("Sending to group {group}"), "encrypted tunnel".into(), true),
                Row::Retry => return,
            };
            p.title = title;
            p.subtitle = subtitle;
            p.phase = Phase::Sending;
            p.hover = None;
            let list: Vec<Xfer> = p
                .files
                .iter()
                .map(|f| Xfer {
                    name: basename(f),
                    sent: Arc::new(AtomicU64::new(0)),
                    total: Arc::new(AtomicU64::new(0)),
                    indeterminate,
                    state: XferState::Queued,
                })
                .collect();
            *p.xfers.lock().unwrap() = list;
            (p.files.clone(), p.xfers.clone())
        };
        let destination = match &row {
            Row::Lan { ip, port, .. } => format!("{ip}:{port}"),
            Row::Tunnel { group } => format!("group {group} via tunnel"),
            Row::Retry => String::new(),
        };
        crate::activity::write(format!("queued {} file(s) for {destination}", files.len()));

        handle.spawn(async move {
            for (i, path) in files.iter().enumerate() {
                let (sent, total) = {
                    let mut xf = xfers.lock().unwrap();
                    let Some(x) = xf.get_mut(i) else { continue };
                    x.state = XferState::Sending;
                    (x.sent.clone(), x.total.clone())
                };
                println!("[dropzone] sending {path} -> {destination}…");
                let res = match &row {
                    Row::Lan { ip, port, .. } => {
                        crate::sender::send_file_with_progress(ip, *port, path, sent, Some(total)).await
                    }
                    Row::Tunnel { group } => match crate::relay_url() {
                        Some(relay) => crate::tunnel::send_over_tunnel(&relay, group, path).await,
                        None => Err("tunnel not configured".into()),
                    },
                    Row::Retry => Err(String::new()),
                };
                let mut xf = xfers.lock().unwrap();
                if let Some(x) = xf.get_mut(i) {
                    match res {
                        Ok(n) => {
                            x.sent.store(n, Ordering::Relaxed);
                            x.total.store(n.max(1), Ordering::Relaxed);
                            x.state = XferState::Done;
                            let msg = format!("Sent {} ({} bytes) to {}", x.name, n, destination);
                            println!("  ↑ {msg}");
                            crate::activity::write(&msg);
                        }
                        Err(e) => {
                            x.state = XferState::Failed(e.clone());
                            let msg = format!("Could not send {} to {}: {}", x.name, destination, e);
                            println!("  ✗ {msg}");
                            crate::activity::write(&msg);
                        }
                    }
                }
            }
        });
    }

    fn fmt_bytes(n: u64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB"];
        let mut v = n as f64;
        let mut i = 0;
        while v >= 1024.0 && i < units.len() - 1 {
            v /= 1024.0;
            i += 1;
        }
        if i > 0 && v < 10.0 {
            format!("{v:.1} {}", units[i])
        } else {
            format!("{} {}", v.round() as u64, units[i])
        }
    }

    /// Draw the whole picker (device list or progress list).
    unsafe fn render_picker(p: &Picker, t: f64) {
        let sc = scale();
        let (w, h) = (p.w, p.h);
        let mut c = Canvas::new(w, h);
        let rad = glass::RADIUS * sc;
        c.rrect(0.0, 0.0, w as f64, h as f64, rad, glass::GLASS, glass::GLASS, 1.0);
        c.rrect_stroke(0.0, 0.0, w as f64, h as f64, rad, glass::LINE, 1.0);

        // header mark + titles
        let pad = PAD * sc;
        c.glow(pad, pad, HEAD_H * sc, HEAD_H * sc, 13.0 * sc, 12.0 * sc, glass::with_alpha(glass::ACCENT_B, 110), 1.0);
        c.rrect(pad, pad, HEAD_H * sc, HEAD_H * sc, 13.0 * sc, glass::ACCENT_A, glass::ACCENT_B, 1.0);
        {
            // up arrow
            let (ax, ay) = (pad + HEAD_H * sc / 2.0, pad + HEAD_H * sc / 2.0);
            let white = (255, 255, 255, 255);
            c.line(ax, ay - 7.0 * sc, ax, ay + 7.0 * sc, 2.2 * sc, white);
            c.line(ax - 6.0 * sc, ay - 1.0 * sc, ax, ay - 7.0 * sc, 2.2 * sc, white);
            c.line(ax + 6.0 * sc, ay - 1.0 * sc, ax, ay - 7.0 * sc, 2.2 * sc, white);
        }
        let tx = px(PAD + HEAD_H + 12.0);
        let tw = w - tx - px(PAD + 30.0);
        c.text(&TextRun::new(&p.title, tx, px(PAD), tw, px(22.0), px(16.0)).bold(700));
        c.text(&TextRun::new(&p.subtitle, tx, px(PAD + 22.0), tw, px(16.0), px(11.5)).color(glass::MUTED));
        // close button
        {
            let (cx, cy, r) = (w as f64 - pad - 12.0 * sc, pad + 12.0 * sc, 12.0 * sc);
            let a = if p.hover_close { 56 } else { 30 };
            c.rrect(cx - r, cy - r, 2.0 * r, 2.0 * r, r, (255, 255, 255, a), (255, 255, 255, a), 1.0);
            let k = 3.5 * sc;
            let col = (255, 255, 255, 190);
            c.line(cx - k, cy - k, cx + k, cy + k, 1.6 * sc, col);
            c.line(cx - k, cy + k, cx + k, cy - k, 1.6 * sc, col);
        }

        let list_x = px(PAD);
        let list_w = w - 2 * px(PAD);
        let mut y = list_top();
        match p.phase {
            Phase::Choose => {
                let foot_top = h - px(14.0 + FOOT_H);
                for (i, row) in p.rows.iter().enumerate() {
                    if y + px(ROW_H) > foot_top - px(8.0) {
                        break;
                    }
                    let hov = p.hover == Some(i);
                    let (fill, stroke) = if hov {
                        ((99, 102, 241, 72), (139, 92, 246, 115))
                    } else {
                        ((255, 255, 255, 18), (255, 255, 255, 18))
                    };
                    c.rrect(list_x as f64, y as f64, list_w as f64, ROW_H * sc, 14.0 * sc, fill, fill, 1.0);
                    c.rrect_stroke(list_x as f64, y as f64, list_w as f64, ROW_H * sc, 14.0 * sc, stroke, 1.0);
                    let (name, meta, tag, av_text, av_col): (String, String, &str, String, (u8, u8, u8)) = match row {
                        Row::Lan { name, ip, port } => (name.clone(), format!("{ip}:{port}"), "LAN", glass::monogram(name), glass::hue_for(name)),
                        Row::Tunnel { group } => (format!("Group {group}"), "every device in the group · encrypted tunnel".into(), "TUNNEL", String::new(), (139, 92, 246)),
                        Row::Retry => ("No devices yet".into(), "open WhisperDrop on the other machine, then tap to retry".into(), "", String::new(), (100, 104, 120)),
                    };
                    // avatar
                    let (ax, ay, asz) = (list_x as f64 + 12.0 * sc, y as f64 + 10.0 * sc, 36.0 * sc);
                    let top: Rgba = (av_col.0, av_col.1, av_col.2, 255);
                    let bot: Rgba = (av_col.0, av_col.1, av_col.2, 140);
                    c.rrect(ax, ay, asz, asz, 12.0 * sc, top, bot, 1.0);
                    if av_text.is_empty() {
                        // globe-ish glyph for group / retry rows
                        let (gx, gy) = (ax + asz / 2.0, ay + asz / 2.0);
                        let white = (255, 255, 255, 235);
                        c.ring(gx, gy, 9.0 * sc, 1.8 * sc, white, None);
                        c.line(gx - 9.0 * sc, gy, gx + 9.0 * sc, gy, 1.6 * sc, white);
                        c.line(gx, gy - 9.0 * sc, gx, gy + 9.0 * sc, 1.6 * sc, white);
                    } else {
                        c.text(&TextRun::new(&av_text, ax as i32, ay as i32, asz as i32, asz as i32, px(15.0)).bold(800).align(glass::ALIGN_CENTER));
                    }
                    // tag + chevron
                    let mut right = list_x + list_w - px(12.0);
                    {
                        let (chx, chy) = ((right - px(7.0)) as f64, y as f64 + ROW_H * sc / 2.0);
                        let col = (255, 255, 255, 100);
                        c.line(chx - 3.0 * sc, chy - 5.0 * sc, chx + 2.0 * sc, chy, 1.8 * sc, col);
                        c.line(chx + 2.0 * sc, chy, chx - 3.0 * sc, chy + 5.0 * sc, 1.8 * sc, col);
                    }
                    right -= px(22.0);
                    if !tag.is_empty() {
                        let tag_w = px(10.0 + 6.4 * tag.len() as f64);
                        c.rrect((right - tag_w) as f64, y as f64 + 20.0 * sc, tag_w as f64, 16.0 * sc, 8.0 * sc, (255, 255, 255, 30), (255, 255, 255, 30), 1.0);
                        c.text(&TextRun::new(tag, right - tag_w, y + px(20.0), tag_w, px(16.0), px(9.5)).bold(700).color((255, 255, 255, 190)).align(glass::ALIGN_CENTER));
                        right -= tag_w + px(10.0);
                    }
                    let nx = list_x + px(12.0 + 36.0 + 12.0);
                    let nw = right - nx;
                    c.text(&TextRun::new(&name, nx, y + px(9.0), nw, px(18.0), px(13.5)).bold(600));
                    c.text(&TextRun::new(&meta, nx, y + px(28.0), nw, px(15.0), px(11.0)).color(glass::MUTED));
                    y += px(ROW_H + ROW_GAP);
                }
                // footer: refresh
                let fa = if p.hover_refresh { 40 } else { 23 };
                c.rrect(list_x as f64, foot_top as f64, list_w as f64, FOOT_H * sc, 11.0 * sc, (255, 255, 255, fa), (255, 255, 255, fa), 1.0);
                c.rrect_stroke(list_x as f64, foot_top as f64, list_w as f64, FOOT_H * sc, 11.0 * sc, glass::LINE, 1.0);
                c.text(&TextRun::new("Refresh devices", list_x, foot_top, list_w, px(FOOT_H), px(12.5)).bold(600).align(glass::ALIGN_CENTER));
            }
            Phase::Sending => {
                let xf = p.xfers.lock().unwrap();
                for x in xf.iter() {
                    if y + px(XFER_H) > h - px(14.0) {
                        break;
                    }
                    c.rrect(list_x as f64, y as f64, list_w as f64, XFER_H * sc, 14.0 * sc, (255, 255, 255, 15), (255, 255, 255, 15), 1.0);
                    c.rrect_stroke(list_x as f64, y as f64, list_w as f64, XFER_H * sc, 14.0 * sc, (255, 255, 255, 18), 1.0);
                    // file icon: small document
                    let (ix, iy, isz) = (list_x as f64 + 12.0 * sc, y as f64 + 10.0 * sc, 30.0 * sc);
                    c.rrect(ix, iy, isz, isz, 9.0 * sc, (255, 255, 255, 26), (255, 255, 255, 26), 1.0);
                    let dc = (255, 255, 255, 200);
                    c.rrect(ix + 9.0 * sc, iy + 7.0 * sc, 12.0 * sc, 16.0 * sc, 2.0 * sc, dc, dc, 0.9);
                    let lc = (20, 22, 30, 200);
                    c.line(ix + 12.0 * sc, iy + 13.0 * sc, ix + 18.0 * sc, iy + 13.0 * sc, 1.2 * sc, lc);
                    c.line(ix + 12.0 * sc, iy + 16.5 * sc, ix + 18.0 * sc, iy + 16.5 * sc, 1.2 * sc, lc);
                    c.line(ix + 12.0 * sc, iy + 20.0 * sc, ix + 16.0 * sc, iy + 20.0 * sc, 1.2 * sc, lc);

                    let sent = x.sent.load(Ordering::Relaxed);
                    let total = x.total.load(Ordering::Relaxed);
                    let (pct_text, meta, pct_col, bar_a, bar_b, frac): (String, String, Rgba, Rgba, Rgba, Option<f64>) = match &x.state {
                        XferState::Queued => ("0%".into(), "queued".into(), (255, 255, 255, 190), glass::ACCENT_A, glass::ACCENT_B, Some(0.0)),
                        XferState::Sending => {
                            if x.indeterminate || total == 0 {
                                ("…".into(), "sending…".into(), (255, 255, 255, 190), glass::ACCENT_A, glass::ACCENT_B, None)
                            } else {
                                let f = sent as f64 / total as f64;
                                (format!("{}%", (f * 100.0).floor() as u64), format!("{} / {}", fmt_bytes(sent), fmt_bytes(total)), (255, 255, 255, 190), glass::ACCENT_A, glass::ACCENT_B, Some(f))
                            }
                        }
                        XferState::Done => (String::new(), "sent".into(), glass::OK, glass::OK, glass::OK, Some(1.0)),
                        XferState::Failed(_) => (String::new(), "failed".into(), glass::BAD, glass::BAD, glass::BAD, Some(1.0)),
                    };
                    let nx = list_x + px(12.0 + 30.0 + 10.0);
                    let pw = px(48.0);
                    let nw = list_x + list_w - px(12.0) - pw - nx;
                    c.text(&TextRun::new(&x.name, nx, y + px(8.0), nw, px(18.0), px(13.0)).bold(600));
                    let meta_text = match &x.state {
                        XferState::Failed(e) if !e.is_empty() => format!("failed · {e}"),
                        _ => meta,
                    };
                    let meta_col = if matches!(x.state, XferState::Failed(_)) { glass::BAD } else { glass::MUTED };
                    c.text(&TextRun::new(&meta_text, nx, y + px(26.0), nw, px(14.0), px(10.5)).color(meta_col));
                    if pct_text.is_empty() {
                        let (kx, ky) = ((list_x + list_w - px(12.0)) as f64 - 8.0 * sc, y as f64 + 17.0 * sc);
                        if x.state == XferState::Done {
                            c.line(kx - 7.0 * sc, ky, kx - 2.5 * sc, ky + 4.5 * sc, 2.2 * sc, pct_col);
                            c.line(kx - 2.5 * sc, ky + 4.5 * sc, kx + 6.0 * sc, ky - 5.0 * sc, 2.2 * sc, pct_col);
                        } else {
                            c.line(kx - 5.0 * sc, ky - 5.0 * sc, kx + 5.0 * sc, ky + 5.0 * sc, 2.2 * sc, pct_col);
                            c.line(kx - 5.0 * sc, ky + 5.0 * sc, kx + 5.0 * sc, ky - 5.0 * sc, 2.2 * sc, pct_col);
                        }
                    } else {
                        c.text(&TextRun::new(&pct_text, list_x + list_w - px(12.0) - pw, y + px(8.0), pw, px(18.0), px(12.0)).bold(700).color(pct_col).align(glass::ALIGN_RIGHT));
                    }
                    c.bar(list_x as f64 + 12.0 * sc, y as f64 + 48.0 * sc, list_w as f64 - 24.0 * sc, 6.0 * sc, frac, bar_a, bar_b, t / 1.2);
                    y += px(XFER_H + ROW_GAP);
                }
            }
        }
        glass::gdi::present(p.hwnd, &c, p.x, p.y);
    }

    unsafe extern "system" fn picker_proc(hwnd: HWND, msg: UINT, w: usize, l: isize) -> isize {
        let (lx, ly) = ((l & 0xFFFF) as i16 as i32, ((l >> 16) & 0xFFFF) as i16 as i32);
        match msg {
            WM_TIMER => {
                picker_tick(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                picker_mouse(hwnd, lx, ly, false);
                0
            }
            WM_LBUTTONUP => {
                picker_mouse(hwnd, lx, ly, true);
                0
            }
            WM_DESTROY => 0,
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    // ---------------------------------------------------------- public API
    pub fn set_side(position: &str) {
        *SIDE.lock().unwrap() = position.to_string();
        // Rebuild on the UI thread (also closes an open picker).
        let hwnd = ANY_HWND.load(Ordering::Relaxed) as HWND;
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd, WM_APP_RESYNC, 0, 0);
            }
        }
    }

    pub fn start_with_port(position: &str, handle: tokio::runtime::Handle, port: u16) {
        STRIP_PORT.store(port, Ordering::Relaxed);
        start(position, handle);
    }

    pub fn start(position: &str, handle: tokio::runtime::Handle) {
        *SIDE.lock().unwrap() = position.to_string();
        *RT_HANDLE.lock().unwrap() = Some(handle);
        std::thread::spawn(|| unsafe {
            SetProcessDPIAware();
            // LOGPIXELSX (88) — available on every Windows version
            let dc = GetDC(0);
            let dpi = GetDeviceCaps(dc, 88);
            ReleaseDC(0, dc);
            if dpi > 0 {
                SCALE_MILLI.store((dpi as u64 * 1000) / 96, Ordering::Relaxed);
            }
            if !CLASSES_READY.swap(true, Ordering::Relaxed) {
                for (name, proc_) in [("WhisperDropStrip", strip_proc as unsafe extern "system" fn(HWND, UINT, usize, isize) -> isize), ("WhisperDropPicker", picker_proc)] {
                    let class_name = glass::wide(name);
                    let class = WNDCLASSW {
                        style: 0,
                        proc_: Some(proc_),
                        extra_cls: 0,
                        extra_win: 0,
                        instance: GetModuleHandleW(std::ptr::null()),
                        icon: 0,
                        cursor: 0,
                        background: 0,
                        menu: std::ptr::null(),
                        class_name: class_name.as_ptr(),
                    };
                    RegisterClassW(&class);
                }
            }
            sync_strips();
            println!("[dropzone] edge strips ready — drag files onto any screen edge to send");

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
    }

}

#[cfg(windows)]
pub use imp::{set_side, start_with_port};

#[cfg(not(windows))]
pub fn start(_position: &str, _handle: tokio::runtime::Handle) {}
#[cfg(not(windows))]
pub fn set_side(_position: &str) {}
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn start_with_port(_position: &str, _handle: tokio::runtime::Handle, _port: u16) {}
