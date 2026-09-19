mod activity;
mod app_config;
mod client;
mod edge;
mod mdns;
mod server;
mod tray;
mod tunnel;

use app_config::Config;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

fn semver_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(candidate), parse(current));
    for i in 0..3 {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x > y {
            return true;
        }
        if x < y {
            return false;
        }
    }
    false
}

/// Fetch <updates_url> (latest.json) and compare with the running version.
/// Returns Some((version, url)) when an update is available.
pub async fn check_for_updates(app: &AppHandle) -> Result<Option<(String, String)>, String> {
    let url = get_cfg().updates_url;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let text = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("{e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{e}"))?;
    let latest = v["version"].as_str().unwrap_or("").to_string();
    let download = v["macos"]["url"].as_str().unwrap_or_default().to_string();
    let current = env!("CARGO_PKG_VERSION");
    if semver_newer(&latest, current) {
        let _ = app;
        Ok(Some((latest, download)))
    } else {
        Ok(None)
    }
}

#[tauri::command]
fn set_position(app: AppHandle, position: String) -> Result<(), String> {
    if position != "left" && position != "right" {
        return Err("invalid position".into());
    }
    let mut cfg = get_cfg();
    cfg.position = position;
    app_config::save(&cfg)?;
    edge::sync(&app, &cfg.position);
    Ok(())
}

/// Is the pointer currently over this window? Resizing a window during a
/// drag makes AppKit emit a spurious drag-leave; the page uses this to tell
/// that artifact apart from the cursor really leaving.
#[tauri::command]
fn edge_cursor_inside(window: tauri::WebviewWindow) -> bool {
    let (Ok(cur), Ok(pos), Ok(size)) = (
        window.cursor_position(),
        window.outer_position(),
        window.outer_size(),
    ) else {
        return false;
    };
    let (x, y) = (cur.x as i32, cur.y as i32);
    x >= pos.x - 2
        && x <= pos.x + size.width as i32 + 2
        && y >= pos.y - 2
        && y <= pos.y + size.height as i32 + 2
}

/// Page → Rust: resize this edge window for the requested UI mode.
#[tauri::command]
fn set_edge_mode(window: tauri::WebviewWindow, mode: String) -> Result<(), String> {
    let mode = edge::Mode::parse(&mode).ok_or("invalid mode")?;
    edge::layout(&window, &get_cfg().position, mode);
    Ok(())
}

#[tauri::command]
fn open_downloads() -> Result<(), String> {
    let dir = crate::server::receive_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&dir).spawn().map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer").arg(&dir).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static CFG: Mutex<Option<Config>> = Mutex::new(None);
static TUNNEL_TASK: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);
static TUNNEL_GEN: AtomicU64 = AtomicU64::new(0);

pub fn get_cfg() -> Config {
    CFG.lock().unwrap().clone().unwrap_or_default()
}

/// Save + apply: reposition edge window, restart tunnel.
fn apply_config(app: &AppHandle, cfg: &Config) {
    *CFG.lock().unwrap() = Some(cfg.clone());
    edge::sync(app, &cfg.position);

    // tunnel: abort old loop, spawn new if enabled
    if let Some(handle) = TUNNEL_TASK.lock().unwrap().take() {
        handle.abort();
    }
    tunnel::TUNNEL_PEERS.lock().unwrap().clear();
    if cfg.tunnel.enabled {
        TUNNEL_GEN.fetch_add(1, Ordering::Relaxed);
        let relay = cfg.tunnel.relay.clone();
        let group = cfg.group_id.clone();
        let id = cfg.tunnel.device_id.clone();
        let app_h = app.clone();
        let task = tauri::async_runtime::spawn(tunnel::receive_loop(relay, group, id, app_h));
        *TUNNEL_TASK.lock().unwrap() = Some(task);
    }
}

pub(crate) fn open_wizard_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("wizard") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = tauri::WebviewWindowBuilder::new(
        app,
        "wizard",
        tauri::WebviewUrl::App("wizard.html".into()),
    )
    .title("WhisperDrop Setup")
    .inner_size(560.0, 700.0)
    .center()
    .resizable(false)
    .build();
}

#[tauri::command]
fn open_wizard(app: AppHandle) {
    open_wizard_window(&app);
}

#[tauri::command]
fn pairing_qr() -> Result<String, String> {
    let cfg = get_cfg();
    let code = format!("wd1|{}|{}", cfg.tunnel.relay, cfg.tunnel.shared_secret);
    Ok(qr_svg(&code))
}

#[tauri::command]
fn apply_pairing(app: AppHandle, code: String) -> Result<(), String> {
    let parts: Vec<&str> = code.trim().split('|').collect();
    if parts.len() != 3 || parts[0] != "wd1" {
        return Err("invalid pairing code".into());
    }
    let mut cfg = get_cfg();
    cfg.tunnel.relay = parts[1].to_string();
    cfg.tunnel.shared_secret = parts[2].to_string();
    cfg.tunnel.enabled = true;
    app_config::save(&cfg)?;
    apply_config(&app, &cfg);
    Ok(())
}

fn qr_svg(text: &str) -> String {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium)
        .unwrap_or_else(|_| QrCode::encode_text("error", QrCodeEcc::Low).unwrap());
    let n = qr.size() as i32;
    let border = 2;
    let dim = n + border * 2;
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + border, y + border));
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" shape-rendering=\"crispEdges\" width=\"240\" height=\"240\"><rect width=\"100%\" height=\"100%\" fill=\"#f2f4fa\"/><path d=\"{path}\" fill=\"#10141f\"/></svg>"
    )
}

#[tauri::command]
async fn check_updates(app: AppHandle) -> Result<serde_json::Value, String> {
    match check_for_updates(&app).await {
        Ok(Some((version, url))) => {
            tray::notify(
                &app,
                "WhisperDrop update available",
                &format!("Version {version} — {url}"),
            );
            Ok(json!({"update": true, "version": version, "url": url}))
        }
        Ok(None) => Ok(json!({"update": false})),
        Err(e) => Err(e),
    }
}

#[tauri::command]
fn get_config() -> Config {
    get_cfg()
}

#[tauri::command]
fn get_tunnel_peers() -> Vec<String> {
    let cfg = get_cfg();
    let mut peers = tunnel::TUNNEL_PEERS.lock().unwrap().clone();
    peers.extend(cfg.known_tunnel_devices);
    peers.retain(|id| id != &cfg.tunnel.device_id && !id.trim().is_empty());
    peers.sort();
    peers.dedup();
    peers
}

#[tauri::command]
async fn refresh_tunnel_peers() -> Result<Vec<String>, String> {
    let cfg = get_cfg();
    if !cfg.tunnel.enabled {
        return Ok(Vec::new());
    }
    let _ = tunnel::devices(&cfg.tunnel.relay, &cfg.group_id, &cfg.tunnel.device_id).await;
    Ok(get_tunnel_peers())
}

#[tauri::command]
async fn wizard_check_network() -> Result<serde_json::Value, String> {
    let probe = tokio::task::spawn_blocking(|| {
        std::net::UdpSocket::bind(("0.0.0.0", 0))
            .and_then(|s| {
                s.connect(("8.8.8.8", 80))
                    .or_else(|_| s.connect(("10.255.255.255", 1)))?;
                s.local_addr()
            })
            .map(|a| a.ip().to_string())
            .map_err(|e| format!("no network: {e}"))
    });
    let ip = tokio::time::timeout(std::time::Duration::from_secs(5), probe)
        .await
        .map_err(|e| format!("timeout: {e}"))?
        .map_err(|e| format!("join: {e}"))?;
    let mut port_ok = false;
    for port in [51731u16, 51730] {
        if tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .is_ok()
        {
            port_ok = true;
            break;
        }
    }
    Ok(serde_json::json!({ "ip": ip, "port_ok": port_ok }))
}

#[tauri::command]
async fn wizard_check_tunnel(relay: String, group_id: String) -> Result<Vec<String>, String> {
    tunnel::check_relay(&relay, &group_id).await?;
    Ok(tunnel::TUNNEL_PEERS.lock().unwrap().clone())
}

#[tauri::command]
async fn wizard_finish(
    app: AppHandle,
    position: String,
    tunnel_enabled: bool,
    relay: String,
    device_id: String,
    shared_secret: String,
    known_tunnel_devices: String,
    receive_dir: String,
) -> Result<(), String> {
    let mut cfg = get_cfg();
    cfg.position = position;
    cfg.tunnel.enabled = tunnel_enabled;
    cfg.tunnel.relay = relay;
    cfg.tunnel.device_id = device_id;
    cfg.tunnel.shared_secret = shared_secret;
    cfg.known_tunnel_devices = known_tunnel_devices
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if !receive_dir.trim().is_empty() {
        cfg.receive_dir = receive_dir;
    }
    cfg.wizard_done = true;
    app_config::save(&cfg)?;
    apply_config(&app, &cfg);
    Ok(())
}

#[tauri::command]
async fn send_file_tunnel(
    app: AppHandle,
    transfer_id: String,
    target: String,
    file_path: String,
) -> Result<u64, String> {
    use serde_json::json;
    let cfg = get_cfg();
    if !cfg.tunnel.enabled {
        return Err("tunnel is disabled".into());
    }
    if cfg.group_id.is_empty() {
        return Err("no group id set — finish the setup wizard first".into());
    }
    let filename = std::path::Path::new(&file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.clone());
    tray::notify(
        &app,
        "Transfer started",
        &format!("Sending {filename} via tunnel…"),
    );

    let sent = Arc::new(AtomicU64::new(0));
    let total = tokio::fs::metadata(&file_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    // live progress → the widget's progress bars (same event as LAN sends)
    let watcher_app = app.clone();
    let watcher_id = transfer_id.clone();
    let watcher_sent = sent.clone();
    let watcher = tauri::async_runtime::spawn(async move {
        let mut last = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let cur = watcher_sent.load(Ordering::Relaxed);
            if cur != last {
                let _ = watcher_app.emit(
                    "transfer-progress",
                    json!({"transferId": watcher_id, "sent": cur, "total": total}),
                );
                last = cur;
            }
        }
    });

    let result = tunnel::send_over_tunnel(
        &cfg.tunnel.relay,
        &cfg.group_id,
        "mac",
        &file_path,
        sent,
    )
    .await;
    watcher.abort();

    match &result {
        Ok(n) => {
            let msg = format!("Sent {filename} ({n} bytes) via tunnel");
            activity::write(&msg);
            tray::notify(&app, "Transfer complete", &msg);
            let _ = app.emit(
                "transfer-progress",
                json!({"transferId": transfer_id, "sent": n, "total": n}),
            );
        }
        Err(e) => {
            let msg = format!("Tunnel send failed for {filename}: {e}");
            activity::write(&msg);
            tray::notify(&app, "Transfer failed", &e);
        }
    }
    result.map(|n| n as u64)
}

#[tauri::command]
async fn send_file(
    app: AppHandle,
    transfer_id: String,
    target_ip: String,
    target_port: u16,
    file_path: String,
) -> Result<String, String> {
    let filename = std::path::Path::new(&file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.clone());
    tray::notify(&app, "Transfer started", &format!("Sending {filename}…"));
    match client::send_file_with_progress(&app, &transfer_id, &target_ip, target_port, &file_path)
        .await
    {
        Ok(bytes) => {
            let msg = format!("Sent {filename} ({bytes} bytes)");
            activity::write(&msg);
            tray::notify(&app, "Transfer complete", &msg);
            Ok(msg)
        }
        Err(e) => {
            activity::write(format!("LAN send failed for {filename}: {e}"));
            tray::notify(&app, "Transfer failed", &e);
            Err(e)
        }
    }
}

#[tauri::command]
fn get_online_peers() -> Vec<mdns::Peer> {
    mdns::online_peers()
}

/// UI-side debug logging (webview console isn't visible in dev logs).
#[tauri::command]
fn ui_debug(msg: String) {
    eprintln!("[ui] {msg}");
    activity::write(format!("ui: {msg}"));
}

/// Probe a host for a WhisperDrop receiver, scanning the fallback port range.
#[tauri::command]
async fn check_peer(target_ip: String) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    for port in [51730u16, 51731, 51732] {
        let url = format!("http://{target_ip}:{port}/health");
        eprintln!("[check_peer] probing {url}");
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success()
                && resp.text().await.unwrap_or_default().contains("bridge-ok")
            {
                return Ok(port);
            }
        }
    }
    Err(format!(
        "no WhisperDrop receiver answered on {target_ip}:51730-51732"
    ))
}

fn hostname() -> String {
    let h = std::process::Command::new("scutil")
        .arg("--get")
        .arg("ComputerName")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mac".to_string());
    h.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

pub fn run() {
    // single-instance guard: hold an exclusive lock file for the process
    // lifetime; a second launch exits immediately instead of racing.
    let lock_path = app_config::config_path().with_extension("lock");
    let probe = std::env::args()
        .any(|a| a.starts_with("--tunnel-devices") || a.starts_with("--tunnel-send"));
    if !probe {
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => {
                if file.try_lock().is_err() {
                    println!("[app] another WhisperDrop instance is already running — exiting");
                    return;
                }
                std::mem::forget(file); // hold until process exit
            }
            Err(e) => eprintln!("[app] lock file error: {e}"),
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // ---- config ----
            let mut cfg = app_config::load();
            if cfg.tunnel.device_id.is_empty() || cfg.tunnel.device_id == "dev" {
                let short: String = hostname().chars().take(6).collect();
                let rand = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(0)
                    & 0xFF_FFFF;
                cfg.tunnel.device_id = format!("{short}-{rand:06x}");
            }
            if !cfg.wizard_done {
                app_config::save(&cfg).ok();
            }
            *CFG.lock().unwrap() = Some(cfg.clone());
            activity::write(format!("started; tunnel={}", cfg.tunnel.enabled));

            // ---- receiver server + mDNS ----
            tauri::async_runtime::spawn(async move {
                let port = server::start_receiver_server().await;
                let hostname = hostname();
                mdns::start(&hostname, port);
            });

            // ---- edge windows: one per monitor, configured side ----
            edge::sync(&handle, &cfg.position);
            edge::watch_monitors(handle.clone());

            // ---- tunnel ----
            if cfg.tunnel.enabled {
                let relay = cfg.tunnel.relay.clone();
                let group = cfg.group_id.clone();
                let id = cfg.tunnel.device_id.clone();
                let app_h = handle.clone();
                let task = tauri::async_runtime::spawn(tunnel::receive_loop(relay, group, id, app_h));
                *TUNNEL_TASK.lock().unwrap() = Some(task);
            }

            // ---- update check shortly after startup ----
            {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    if let Ok(Some((version, url))) = check_for_updates(&h).await {
                        tray::notify(
                            &h,
                            "WhisperDrop update available",
                            &format!("Version {version} — {url}"),
                        );
                    }
                });
            }

            // ---- first-run: open the wizard ----
            if !cfg.wizard_done {
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    open_wizard_window(&h);
                });
            }

            tray::setup(&handle)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_file,
            send_file_tunnel,
            set_edge_mode,
            edge_cursor_inside,
            get_online_peers,
            get_config,
            get_tunnel_peers,
            refresh_tunnel_peers,
            ui_debug,
            check_peer,
            wizard_check_network,
            wizard_check_tunnel,
            wizard_finish,
            open_wizard,
            check_updates,
            pairing_qr,
            apply_pairing,
            set_position,
            open_downloads,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running WhisperDrop");
}
