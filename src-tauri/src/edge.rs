//! Edge drop-zone windows.
//!
//! One transparent, always-on-top window per monitor, docked to the
//! configured screen side. The full monitor height is a drop target; the
//! page draws a slim pill in idle mode and grows into a card / panel on
//! demand. Windows are labelled `edge-<monitor index>` and re-synced when
//! monitors are plugged or unplugged. On macOS they join every Space and
//! stay above fullscreen apps.

use tauri::{
    utils::config::WindowEffectsConfig,
    window::{Effect, EffectState},
    AppHandle, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

/// Logical sizes shared with edge.html (keep in sync with its constants).
pub const STRIP_W: f64 = 14.0;
pub const WIDE_W: f64 = 168.0;
pub const PANEL_W: f64 = 340.0;
pub const PANEL_H: f64 = 500.0;
pub const MENU_W: f64 = 200.0;
pub const MENU_H: f64 = 244.0;
pub const RADIUS: f64 = 20.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Strip,
    Wide,
    Panel,
    Menu,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "strip" => Some(Mode::Strip),
            "wide" => Some(Mode::Wide),
            "panel" => Some(Mode::Panel),
            "menu" => Some(Mode::Menu),
            _ => None,
        }
    }
}

pub fn label_for(index: usize) -> String {
    format!("edge-{index}")
}

fn index_of(label: &str) -> Option<usize> {
    label.strip_prefix("edge-")?.parse().ok()
}

/// Stable description of the monitor layout, used to detect hot-plug.
pub fn fingerprint(app: &AppHandle) -> String {
    let mut parts: Vec<String> = app
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            format!("{}x{}@{},{}", s.width, s.height, p.x, p.y)
        })
        .collect();
    parts.sort();
    parts.join("|")
}

fn effects() -> WindowEffectsConfig {
    WindowEffectsConfig {
        effects: vec![Effect::HudWindow, Effect::Acrylic],
        state: Some(EffectState::FollowsWindowActiveState),
        radius: Some(RADIUS),
        color: None,
    }
}

fn create(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    let win = WebviewWindowBuilder::new(app, label, WebviewUrl::App("edge.html".into()))
        .title("WhisperDrop Edge")
        .inner_size(STRIP_W, 600.0)
        .transparent(true)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .accept_first_mouse(true)
        .visible_on_all_workspaces(true)
        .effects(effects())
        .build()
        .map_err(|e| eprintln!("[edge] create {label} failed: {e}"))
        .ok()?;
    #[cfg(target_os = "macos")]
    mac::configure(&win);
    Some(win)
}

/// Physical rect for `mode` on `monitor`.
fn rect(monitor: &Monitor, side: &str, mode: Mode) -> (i32, i32, u32, u32) {
    let wa = monitor.work_area();
    let s = monitor.scale_factor();
    let px = |v: f64| (v * s).round() as u32;
    let (w, h) = match mode {
        Mode::Strip => (px(STRIP_W), wa.size.height),
        Mode::Wide => (px(WIDE_W), wa.size.height),
        Mode::Panel => (px(PANEL_W), px(PANEL_H).min(wa.size.height)),
        Mode::Menu => (px(MENU_W), px(MENU_H).min(wa.size.height)),
    };
    let x = if side == "left" {
        wa.position.x
    } else {
        wa.position.x + wa.size.width as i32 - w as i32
    };
    let y = wa.position.y + (wa.size.height as i32 - h as i32) / 2;
    (x, y, w, h)
}

fn monitor_for(win: &WebviewWindow) -> Option<Monitor> {
    let by_index = index_of(win.label())
        .and_then(|i| win.available_monitors().ok()?.into_iter().nth(i));
    by_index.or_else(|| win.current_monitor().ok().flatten())
}

/// Resize/move `win` for `mode` on its own monitor. Runs as one unit on
/// the main thread so back-to-back mode changes cannot interleave.
pub fn layout(win: &WebviewWindow, side: &str, mode: Mode) {
    let w = win.clone();
    let side = side.to_string();
    let _ = win.run_on_main_thread(move || layout_now(&w, &side, mode));
}

fn layout_now(win: &WebviewWindow, side: &str, mode: Mode) {
    let Some(monitor) = monitor_for(win) else {
        return;
    };
    let (x, y, w, h) = rect(&monitor, side, mode);
    // Right-docked windows grow leftwards: move first so the transient
    // frame never pokes off-screen.
    if side == "left" {
        let _ = win.set_size(PhysicalSize::new(w, h));
        let _ = win.set_position(PhysicalPosition::new(x, y));
    } else {
        let _ = win.set_position(PhysicalPosition::new(x, y));
        let _ = win.set_size(PhysicalSize::new(w, h));
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
    #[cfg(target_os = "macos")]
    {
        mac::set_frost(win, mode != Mode::Strip && mode != Mode::Wide);
        // tao's always-on-top resets the level to "floating"; re-assert ours.
        mac::configure(win);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = win.set_always_on_top(true);
}

/// Create/close windows to match the monitor list and dock every one of
/// them in strip mode on `side`. Pages are told to reset their state.
pub fn sync(app: &AppHandle, side: &str) {
    let h = app.clone();
    let side = side.to_string();
    let _ = app.run_on_main_thread(move || sync_now(&h, &side));
}

fn sync_now(app: &AppHandle, side: &str) {
    let monitors = app.available_monitors().unwrap_or_default();
    for (i, _) in monitors.iter().enumerate() {
        let label = label_for(i);
        let win = match app.get_webview_window(&label) {
            Some(w) => w,
            None => match create(app, &label) {
                Some(w) => w,
                None => continue,
            },
        };
        layout_now(&win, side, Mode::Strip);
        let _ = win.show();
        let _ = tauri::Emitter::emit_to(app, label.as_str(), "edge-reset", side);
    }
    let mut extra = monitors.len();
    while let Some(w) = app.get_webview_window(&label_for(extra)) {
        let _ = w.close();
        extra += 1;
    }
    eprintln!("[edge] {} window(s) docked on the {side}", monitors.len());
}

/// Poll for monitor hot-plug and re-sync the windows when it changes.
pub fn watch_monitors(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = fingerprint(&app);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let now = fingerprint(&app);
            if now != last {
                last = now;
                sync(&app, &crate::get_cfg().position);
            }
        }
    });
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use tauri::WebviewWindow;

    // NSWindowCollectionBehavior
    const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const STATIONARY: usize = 1 << 4;
    const IGNORES_CYCLE: usize = 1 << 6;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    // NSStatusWindowLevel — above fullscreen app content and the Dock.
    const STATUS_LEVEL: isize = 25;
    // Tag used by window-vibrancy for the NSVisualEffectView it inserts.
    const BLUR_VIEW_TAG: isize = 91_376_254;

    fn with_view<F: FnOnce(*mut AnyObject)>(win: &WebviewWindow, f: F) {
        if let Ok(view) = win.ns_view() {
            if !view.is_null() {
                f(view as *mut AnyObject);
            }
        }
    }

    /// Join every Space, float over fullscreen apps, never appear in ⌘-Tab.
    pub fn configure(win: &WebviewWindow) {
        with_view(win, |view| unsafe {
            let window: *mut AnyObject = msg_send![view, window];
            if window.is_null() {
                return;
            }
            let behavior =
                CAN_JOIN_ALL_SPACES | STATIONARY | IGNORES_CYCLE | FULL_SCREEN_AUXILIARY;
            let _: () = msg_send![window, setCollectionBehavior: behavior];
            let _: () = msg_send![window, setLevel: STATUS_LEVEL];
            let _: () = msg_send![window, setHidesOnDeactivate: false];
        });
    }

    /// Show/hide the frosted backdrop. Idle and drag-over modes are pure
    /// CSS on a fully transparent window; the panel and menu use vibrancy.
    pub fn set_frost(win: &WebviewWindow, on: bool) {
        with_view(win, move |view| unsafe {
            let blur: *mut AnyObject = msg_send![view, viewWithTag: BLUR_VIEW_TAG];
            if !blur.is_null() {
                let _: () = msg_send![blur, setHidden: !on];
            }
        });
    }
}
