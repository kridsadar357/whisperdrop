#![cfg_attr(windows, windows_subsystem = "windows")]
//! WhisperDrop — bidirectional LAN/tunnel file transfer.
//! Receives (HTTP + overlay animation) and sends (console commands +
//! drag-drop edge strip on Windows). Peers discovered via mDNS, or relayed
//! across networks through the riki-api.online tunnel with per-device IDs.

mod activity;
mod cli;
mod config;
mod dropzone;
mod glass;
mod mdns;
mod overlay;
mod sender;
mod tunnel;
mod wizard;

const WIZARD_HTML: &str = include_str!("wizard_page.html");
static TUNNEL_TASK: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Query},
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Router,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::io::AsyncWriteExt;

// 51730 turned out to be filtered/dropped on some Windows setups (a stale
// block rule from a cancelled firewall prompt); 51731+ are clean, so the
// receiver defaults there. Senders discover the actual port via mDNS or
// the check_peer port scan, so any port works.
const DEFAULT_PORT: u16 = 51731;

static RUNTIME_CFG: Mutex<Option<config::Config>> = Mutex::new(None);

/// Current tunnel device id (if configured).
pub fn device_id() -> Option<String> {
    RUNTIME_CFG
        .lock()
        .unwrap()
        .as_ref()
        .filter(|c| c.tunnel.enabled)
        .map(|c| c.tunnel.device_id.clone())
}

/// Current relay url (if configured).
pub fn relay_url() -> Option<String> {
    RUNTIME_CFG
        .lock()
        .unwrap()
        .as_ref()
        .filter(|c| c.tunnel.enabled)
        .map(|c| c.tunnel.relay.clone())
}

pub fn known_tunnel_devices() -> Vec<String> {
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_default();
    let self_id = cfg.tunnel.device_id.clone();
    cfg.known_tunnel_devices
        .into_iter()
        .filter(|id| !id.trim().is_empty() && id != &self_id)
        .collect()
}

/// (Re)start the tunnel receive loop — used at boot and by the wizard page.
fn start_tunnel(cfg: &config::Config) {
    if let Some(h) = TUNNEL_TASK.lock().unwrap().take() {
        h.abort();
    }
    tunnel::TUNNEL_PEERS.lock().unwrap().clear();
    if cfg.tunnel.enabled {
        let relay = cfg.tunnel.relay.clone();
        let id = cfg.tunnel.device_id.clone();
        println!("[tunnel] starting as {} via {}", id, relay);
        let h = tokio::spawn(tunnel::receive_loop(relay, id));
        *TUNNEL_TASK.lock().unwrap() = Some(h);
    }
}

static RT: Mutex<Option<tokio::runtime::Handle>> = Mutex::new(None);
pub fn rt_handle() -> Option<tokio::runtime::Handle> {
    RT.lock().unwrap().clone()
}

/// Native Yes/No dialog (Windows) — used for join-request approvals.
#[cfg(windows)]
pub fn win_confirm(title: &str, text: &str) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    const MB_YESNO: u32 = 0x4;
    const MB_ICONQUESTION: u32 = 0x20;
    const MB_TOPMOST: u32 = 0x40000;
    const MB_SETFOREGROUND: u32 = 0x10000;
    const IDYES: i32 = 6;
    unsafe { MessageBoxW(0, w(text).as_ptr(), w(title).as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND) == IDYES }
}

/// Persist a new membership and restart the tunnel with it.
pub fn save_membership(group: &str, token: &str, role: &str) -> Result<(), String> {
    let mut cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    cfg.group_id = group.to_string();
    cfg.tunnel.member_token = token.to_string();
    cfg.tunnel.role = role.to_string();
    cfg.tunnel.pending_request.clear();
    cfg.tunnel.enabled = true;
    config::save(&cfg)?;
    *RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
    tunnel::set_group(cfg.group_id.clone());
    if rt_handle().is_some() && tokio::runtime::Handle::try_current().is_ok() {
        start_tunnel(&cfg);
    }
    activity::write(format!("joined group {group} as {role}"));
    Ok(())
}

/// Apply a join outcome to the config (approved → member, pending → remembered).
pub fn finish_join(group: &str, outcome: &tunnel::JoinOutcome) -> Result<(), String> {
    match outcome {
        tunnel::JoinOutcome::Approved { token } => save_membership(group, token, "member"),
        tunnel::JoinOutcome::Pending { request_id } => {
            let mut cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
            cfg.group_id = group.to_string();
            cfg.tunnel.pending_request = request_id.clone();
            config::save(&cfg)?;
            *RUNTIME_CFG.lock().unwrap() = Some(cfg);
            Ok(())
        }
        tunnel::JoinOutcome::Denied => {
            let mut cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
            cfg.tunnel.pending_request.clear();
            config::save(&cfg)?;
            *RUNTIME_CFG.lock().unwrap() = Some(cfg);
            Ok(())
        }
    }
}

pub fn leave_group() -> Result<(), String> {
    let mut cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    cfg.group_id.clear();
    cfg.tunnel.member_token.clear();
    cfg.tunnel.role.clear();
    cfg.tunnel.pending_request.clear();
    config::save(&cfg)?;
    *RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
    tunnel::set_group(String::new());
    if tokio::runtime::Handle::try_current().is_ok() {
        start_tunnel(&cfg);
    }
    Ok(())
}

// ---- wizard page: group endpoints ----
fn json_ok<T: serde::Serialize>(v: T) -> String {
    serde_json::json!({"ok": true, "result": v}).to_string()
}
fn json_err(e: String) -> String {
    serde_json::json!({"ok": false, "err": e}).to_string()
}

async fn api_group_create() -> String {
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    match tunnel::group_create(&cfg.tunnel.relay, &cfg.tunnel.device_id, &hostname()).await {
        Ok((group, token)) => match save_membership(&group, &token, "head") {
            Ok(()) => json_ok(group),
            Err(e) => json_err(e),
        },
        Err(e) => json_err(e),
    }
}

async fn api_group_join(body: String) -> String {
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let group = v["group"].as_str().unwrap_or("").trim().to_string();
    if group.len() != 6 || !group.chars().all(|c| c.is_ascii_digit()) {
        return json_err("group id must be 6 digits".into());
    }
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    match tunnel::group_join(&cfg.tunnel.relay, &group, &cfg.tunnel.device_id, &hostname(), 25).await {
        Ok(outcome) => match finish_join(&group, &outcome) {
            Ok(()) => json_ok(outcome),
            Err(e) => json_err(e),
        },
        Err(e) => json_err(e),
    }
}

async fn api_group_join_status() -> String {
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    if cfg.tunnel.pending_request.is_empty() {
        return json_err("no pending join request".into());
    }
    match tunnel::group_join_status(&cfg.tunnel.relay, &cfg.group_id, &cfg.tunnel.pending_request).await {
        Ok(outcome) => match finish_join(&cfg.group_id, &outcome) {
            Ok(()) => json_ok(outcome),
            Err(e) => json_err(e),
        },
        Err(e) => json_err(e),
    }
}

async fn api_group_leave() -> String {
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    if !cfg.group_id.is_empty() && !cfg.tunnel.member_token.is_empty() {
        let _ = tunnel::group_query(&cfg.tunnel.relay, &cfg.group_id, serde_json::json!({"type":"leave"}), &["ok"]).await;
    }
    match leave_group() {
        Ok(()) => json_ok(true),
        Err(e) => json_err(e),
    }
}

async fn api_group_query(body: String) -> String {
    // {"frame": {...}, "reply": "members"} — head-only frames are enforced by the relay
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let reply = v["reply"].as_str().unwrap_or("ok").to_string();
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    match tunnel::group_query(&cfg.tunnel.relay, &cfg.group_id, v["frame"].clone(), &[reply.as_str()]).await {
        Ok(r) => json_ok(r),
        Err(e) => json_err(e),
    }
}

async fn api_tunnel_status() -> String {
    let cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    serde_json::json!({
        "status": tunnel::status(), "group": cfg.group_id, "role": cfg.tunnel.role,
        "pending_request": cfg.tunnel.pending_request, "enabled": cfg.tunnel.enabled,
        "member": !cfg.tunnel.member_token.is_empty(),
    })
    .to_string()
}

/// Move the drop zone + overlay to the other screen edge and persist it
/// (used by the strip's context menu).
pub fn set_position(position: &str) {
    let mut cfg = RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(config::load);
    cfg.position = position.to_string();
    if let Err(e) = config::save(&cfg) {
        println!("[config] save failed: {e}");
    }
    *RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
    overlay::set_side(&cfg.position);
    dropzone::set_side(&cfg.position);
    activity::write(format!("drop zone moved to the {position} edge"));
}

fn receive_dir() -> PathBuf {
    if let Some(cfg) = RUNTIME_CFG.lock().unwrap().as_ref() {
        if !cfg.receive_dir.trim().is_empty() {
            return PathBuf::from(&cfg.receive_dir);
        }
    }
    let mut d = dirs::download_dir()
        .or_else(dirs::home_dir)
        .expect("no home directory");
    d.push("BridgeReceived");
    d
}

/// Keep only the final path component; reject empty / dot-names so streams
/// can't escape the receive dir.
pub fn sanitize_filename(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() || name.starts_with('.') {
        return None;
    }
    Some(name.to_string())
}

/// "a.txt" -> "a (1).txt" on collision.
fn unique_path(dir: &std::path::Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = candidate.extension().and_then(|s| s.to_str());
    for i in 1..u32::MAX {
        let name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(name);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

async fn upload(
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    stream: Body,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    println!("→ incoming upload: {:?}", params.get("filename"));
    let filename = sanitize_filename(
        params
            .get("filename")
            .map(String::as_str)
            .unwrap_or("unnamed"),
    )
    .ok_or((StatusCode::BAD_REQUEST, "invalid filename".to_string()))?;

    let dir = receive_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let dest = unique_path(&dir, &filename);

    let file = tokio::fs::File::create(&dest)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // 1 MiB buffer: without it each small network chunk is a separate
    // blocking-pool write (and on Windows a fresh touch for Defender to
    // scan), which throttles LAN throughput to a crawl.
    let mut file = tokio::io::BufWriter::with_capacity(1 << 20, file);
    // Expected size comes from the sender's Content-Length (streamed
    // uploads); without it the overlay shows an indeterminate bar.
    let total = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    overlay::begin(&filename, total);
    let mut stream = stream.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        written += chunk.len() as u64;
        overlay::progress(written, total);
    }
    file.flush()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    overlay::finish();
    println!("✓ received {written} bytes -> {}", dest.display());
    activity::write(format!(
        "received {filename} ({written} bytes) -> {}",
        dest.display()
    ));
    Ok((StatusCode::OK, String::new()))
}

async fn upload_multipart(
    mut multipart: Multipart,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    let dir = receive_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let filename = field
            .file_name()
            .and_then(sanitize_filename)
            .unwrap_or_else(|| "unnamed".to_string());
        let dest = unique_path(&dir, &filename);
        let mut file = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let total = field
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        overlay::begin(&filename, total);
        let mut written: u64 = 0;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            written += chunk.len() as u64;
            overlay::progress(written, total);
        }
        file.flush()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        overlay::finish();
        println!("✓ received {written} bytes -> {}", dest.display());
        activity::write(format!(
            "received {filename} ({written} bytes) -> {}",
            dest.display()
        ));
    }
    Ok((StatusCode::OK, String::new()))
}

async fn health() -> &'static str {
    println!("→ health check received");
    "bridge-ok"
}

// ---- pairing (QR + code) ----
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

fn pairing_code(cfg: &config::Config) -> String {
    format!("wd1|{}|{}", cfg.tunnel.relay, cfg.tunnel.shared_secret)
}

async fn pairing_qr() -> Result<String, (StatusCode, String)> {
    let cfg = config::load();
    Ok(qr_svg(&pairing_code(&cfg)))
}

async fn pairing_apply(body: String) -> Result<String, (StatusCode, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let code = v["code"].as_str().unwrap_or("");
    let parts: Vec<&str> = code.split('|').collect();
    if parts.len() != 3 || parts[0] != "wd1" {
        return Ok(serde_json::json!({"ok": false, "err": "invalid pairing code"}).to_string());
    }
    let mut cfg = config::load();
    cfg.tunnel.enabled = true;
    cfg.tunnel.relay = parts[1].to_string();
    cfg.tunnel.shared_secret = parts[2].to_string();
    cfg.wizard_done = true;
    config::save(&cfg).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    overlay::set_side(&cfg.position);
    dropzone::set_side(&cfg.position);
    start_tunnel(&cfg);
    println!(
        "[pairing] applied pairing code (relay={})",
        cfg.tunnel.relay
    );
    Ok(serde_json::json!({"ok": true}).to_string())
}

// ---- setup wizard (browser form) ----
async fn wizard_state() -> Result<String, (StatusCode, String)> {
    let cfg = config::load();
    let ip = std::net::UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|s| {
            s.connect(("8.8.8.8", 80))
                .or_else(|_| s.connect(("10.255.255.255", 1)))?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut port_ok = false;
    for p in [51731u16, 51730, 51732] {
        if tokio::net::TcpListener::bind(("0.0.0.0", p)).await.is_ok() {
            port_ok = true;
            break;
        }
    }
    let v = serde_json::json!({
        "position": cfg.position,
        "group_id": cfg.group_id,
        "wizard_done": cfg.wizard_done,
        "tunnel": cfg.tunnel,
        "receive_dir": cfg.receive_dir,
        "net": {"ip": ip, "port_ok": port_ok},
    });
    Ok(v.to_string())
}

async fn wizard_test_tunnel(body: String) -> Result<String, (StatusCode, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let relay = v["relay"].as_str().unwrap_or_default();
    let device_id = v["device_id"].as_str().unwrap_or_default();
    match tunnel::check_relay(relay, device_id).await {
        Ok(()) => Ok(serde_json::json!({"ok": true, "ids": []}).to_string()),
        Err(e) => Ok(serde_json::json!({"ok": false, "err": e}).to_string()),
    }
}

async fn activity_log() -> String {
    activity::recent()
}

async fn refresh_devices() -> String {
    let cfg = config::load();
    let mut tunnel = if cfg.tunnel.enabled {
        tunnel::devices(&cfg.tunnel.relay, &cfg.tunnel.device_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    tunnel.extend(cfg.known_tunnel_devices);
    tunnel.sort();
    tunnel.dedup();
    let lan: Vec<serde_json::Value> = mdns::list().into_iter().map(|p| {
        serde_json::json!({"name": p.name, "address": format!("{}:{}", p.ip, p.port), "kind": "LAN"})
    }).collect();
    serde_json::json!({"lan": lan, "tunnel": tunnel}).to_string()
}

async fn wizard_finish(body: String) -> Result<String, (StatusCode, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut cfg = config::load();
    cfg.position = v["position"].as_str().unwrap_or("right").to_string();
    cfg.tunnel.enabled = v["tunnel_enabled"].as_bool().unwrap_or(false);
    cfg.tunnel.relay = v["relay"]
        .as_str()
        .unwrap_or("wss://riki-api.online/ws")
        .to_string();
    cfg.tunnel.device_id = v["device_id"]
        .as_str()
        .unwrap_or(&cfg.tunnel.device_id)
        .to_string();
    cfg.tunnel.shared_secret = v["shared_secret"].as_str().unwrap_or_default().to_string();
    cfg.known_tunnel_devices = v["tunnel_peers"]
        .as_str()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(path) = v["receive_dir"].as_str().filter(|p| !p.trim().is_empty()) {
        cfg.receive_dir = path.to_string();
    }
    cfg.wizard_done = true;
    config::save(&cfg).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // All runtime helpers (receive folder, encryption key and trusted tunnel
    // IDs) read this snapshot, so replace it before restarting the tunnel.
    *RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());


    overlay::set_side(&cfg.position);
    dropzone::set_side(&cfg.position);
    start_tunnel(&cfg);
    activity::write(format!(
        "settings saved; position={}, tunnel={}",
        cfg.position, cfg.tunnel.enabled
    ));
    println!(
        "[wizard] setup saved & applied (position={}, tunnel={})",
        cfg.position, cfg.tunnel.enabled
    );
    Ok(serde_json::json!({"ok": true}).to_string())
}

/// Bind DEFAULT_PORT, falling forward on collision. Returns the bound
/// listener so main can serve it in the foreground — if the server dies,
/// the process dies with it instead of announcing a dead service.
async fn bind_first_free() -> (tokio::net::TcpListener, u16) {
    for port in DEFAULT_PORT..DEFAULT_PORT + 20 {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return (listener, port),
            Err(_) => continue,
        }
    }
    panic!("no free port in {DEFAULT_PORT}..{}", DEFAULT_PORT + 20);
}

fn hostname() -> String {
    let h = std::env::var("COMPUTERNAME")
        .or_else(|_| {
            Ok::<String, std::env::VarError>(
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "pc".to_string()),
            )
        })
        .unwrap_or_else(|_| format!("bridge-{}", std::process::id()));
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

/// Windows is intentionally a background/tray app: the edge strip remains
/// available while the console is hidden in release builds. The tray is the
/// reliable way back to Preferences, received files and the activity log.
#[cfg(windows)]
fn setup_tray(port: u16) -> Option<tray_item::TrayItem> {
    use tray_item::{IconSource, TrayItem};
    use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};

    let fallback = unsafe { LoadIconW(0, IDI_APPLICATION) };
    // The release binary embeds icon resource 1. If a development build was
    // made without windres, retain a usable Windows fallback instead.
    let mut tray = TrayItem::new("WhisperDrop", IconSource::Resource("1"))
        .or_else(|_| TrayItem::new("WhisperDrop", IconSource::RawIcon(fallback)))
        .ok()?;
    let preferences_url = format!("http://127.0.0.1:{port}/wizard");
    tray.add_menu_item("Preferences…", move || {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &preferences_url])
            .spawn();
    })
    .ok()?;
    tray.add_menu_item("Open received files", || {
        let _ = std::process::Command::new("explorer")
            .arg(receive_dir())
            .spawn();
    })
    .ok()?;
    tray.add_menu_item("View activity log", || {
        activity::write("activity log opened from tray");
        let _ = std::process::Command::new("notepad")
            .arg(activity::path())
            .spawn();
    })
    .ok()?;
    tray.add_menu_item("Quit WhisperDrop", || std::process::exit(0))
        .ok()?;
    Some(tray)
}

fn mdns_register(host_name: &str, port: u16) {
    mdns::start(host_name, port);
}

#[cfg(windows)]
mod firewall {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const RULE_NAME: &str = "WhisperDrop Receiver";

    fn current_exe() -> String {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "WhisperDrop.exe".to_string())
    }

    fn run_quiet(cmd: &str, args: &[&str]) -> Option<std::process::Output> {
        std::process::Command::new(cmd)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()
    }

    /// `net session` only succeeds when elevated — the classic check.
    fn is_elevated() -> bool {
        run_quiet("net", &["session"])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Does an allow rule with our name exist *for this exe path*? A rule
    /// left behind by an older/moved binary matches by name but does not
    /// cover the current program, so inbound mDNS/TCP would still be blocked.
    pub(crate) fn rule_exists() -> bool {
        let out = run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={RULE_NAME}"),
                "verbose",
            ],
        )
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
        if !out.contains(RULE_NAME) {
            return false;
        }
        // verbose output lists "Program: <path>" for each rule
        out.to_lowercase().contains(&current_exe().to_lowercase())
    }

    fn delete_rule_elevated() {
        let _ = run_quiet(
            "netsh",
            &["advfirewall", "firewall", "delete", "rule", &format!("name={RULE_NAME}")],
        );
    }

    /// Add an inbound allow rule scoped to this exe (covers TCP 51730+ and
    /// the mDNS UDP traffic). Must be called elevated.
    fn add_rule_elevated() -> bool {
        let exe = current_exe();
        run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={RULE_NAME}"),
                "dir=in",
                "action=allow",
                &format!("program={exe}"),
                "enable=yes",
                "profile=any",
            ],
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
    }

    /// Ensure the inbound rule exists. If we're not elevated, relaunch
    /// ourselves with --setup-firewall through PowerShell's RunAs verb,
    /// which triggers a single UAC prompt.
    pub(crate) fn ensure() {
        if rule_exists() {
            println!("[firewall] rule already present — OK");
            return;
        }
        println!("[firewall] asking Windows for permission (UAC prompt)…");
        if is_elevated() {
            delete_rule_elevated();
            let ok = add_rule_elevated();
            println!(
                "[firewall] {}",
                if ok {
                    "rule added ✓"
                } else {
                    "failed to add rule ✗"
                }
            );
            return;
        }
        let exe = current_exe();
        let ps = format!(
            "Start-Process -FilePath '{}' -ArgumentList '--setup-firewall' -Verb RunAs -Wait",
            exe
        );
        let status = run_quiet(
            "powershell",
            &["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps],
        )
        .map(|o| o.status.success())
        .unwrap_or(false);
        if status && rule_exists() {
            println!("[firewall] rule added ✓ (you approved the UAC prompt)");
        } else {
            println!("[firewall] ⚠ rule NOT added — incoming connections will be blocked.");
            println!("[firewall]   add manually (admin): netsh advfirewall firewall add rule \\");
            println!("[firewall]   name=\"{RULE_NAME}\" dir=in action=allow program=\"{exe}\"");
        }
    }

    /// Entry point when relaunched elevated for setup.
    pub fn setup_and_exit() -> ! {
        // replace any stale rule (old exe name / location) with one for this exe
        delete_rule_elevated();
        let ok = add_rule_elevated();
        println!(
            "[firewall] {}",
            if ok {
                "rule added ✓"
            } else {
                "failed to add rule ✗"
            }
        );
        std::thread::sleep(std::time::Duration::from_millis(1200));
        std::process::exit(if ok { 0 } else { 1 });
    }
}

#[tokio::main]
async fn main() {
    use clap::Parser as _;
    let args = cli::Cli::parse();
    // one-shot subcommands never start the app
    match args.command {
        None | Some(cli::Command::Run) | Some(cli::Command::Setup) => {}
        Some(cmd) => {
            if let Err(e) = cli::run(cmd).await {
                eprintln!("✗ {e}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
    let wizard_requested = args.wizard || matches!(args.command, Some(cli::Command::Setup));

    // single-instance guard (the elevated --setup-firewall child skips it)
    let is_setup = args.setup_firewall;
    if !is_setup {
        let lock_path = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("WhisperDrop")
            .join("app.lock");
        match std::fs::OpenOptions::new().create(true).write(true).open(&lock_path) {
            Ok(file) => {
                if file.try_lock().is_err() {
                    println!("[app] another WhisperDrop instance is already running — exiting");
                    return;
                }
                std::mem::forget(file);
            }
            Err(e) => eprintln!("[app] lock file error: {e}"),
        }
    }
    println!("========================================");
    println!(" WhisperDrop  —  send & receive");
    println!("========================================");

    // --setup-firewall (relaunched elevated by the wizard/firewall step)
    #[cfg(windows)]
    {
        let setup = std::env::args().any(|a| a == "--setup-firewall");
        if setup {
            firewall::setup_and_exit();
        }
    }

    // ---- config + first-run wizard ----
    let cfg = config::load();
    let show_wizard = !cfg.wizard_done || wizard_requested;
    *RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
    activity::write(format!(
        "WhisperDrop started; tunnel={}",
        cfg.tunnel.enabled
    ));
    overlay::set_side(&cfg.position);

    #[cfg(windows)]
    firewall::ensure();

    // ---- receiver server + mDNS announce ----
    let (listener, port) = bind_first_free().await;
    let host = hostname();
    println!(" device name : {host}");
    println!(" listening   : 0.0.0.0:{port}");
    println!(" saving to   : {}", receive_dir().display());
    println!(" drop zone   : {} edge", cfg.position);
    println!("========================================");
    mdns_register(&host, port);

    // ---- tunnel (cross-network) ----
    start_tunnel(&cfg);
    tunnel::set_group(cfg.group_id.clone());

    // ---- update check (compare against the hosted manifest) ----
    {
        let updates_url = cfg.updates_url.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let client = match reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            if let Ok(text) = client.get(&updates_url).send().and_then(|r| r.text()) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let latest = v["version"].as_str().unwrap_or("");
                    let current = env!("CARGO_PKG_VERSION");
                    let newer = latest
                        .split('.')
                        .zip(current.split('.'))
                        .find(|(a, b)| {
                            a.parse::<u64>().unwrap_or(0) != b.parse::<u64>().unwrap_or(0)
                        })
                        .map(|(a, b)| a.parse::<u64>().unwrap_or(0) > b.parse::<u64>().unwrap_or(0))
                        .unwrap_or(false);
                    if newer {
                        let url = v["windows"]["url"].as_str().unwrap_or("(see release page)");
                        let msg = format!("Update available: v{latest} — {url}");
                        println!("[update] {msg}");
                        crate::activity::write(&msg);
                    }
                }
            }
        });
    }

    // ---- first run: open the setup wizard in the default browser ----
    if show_wizard {
        let url = format!("http://127.0.0.1:{port}/wizard");
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(800));
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "start", "", &url])
                    .spawn();
            }
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open").arg(&url).spawn();
            }
            #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
            {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }
        });
    }

    *RT.lock().unwrap() = Some(tokio::runtime::Handle::current());

    // ---- drag-drop edge strip (Windows) ----
    #[cfg(windows)]
    dropzone::start_with_port(&cfg.position, tokio::runtime::Handle::current(), port);

    // --demo-overlay: play a fake transfer animation for previewing the UI
    if std::env::args().any(|a| a == "--demo-overlay") {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(3000));
            overlay::begin("demo-photo.jpg", 100);
            for p in 0..=100 {
                overlay::progress(p, 100);
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            overlay::finish();
            std::thread::sleep(std::time::Duration::from_millis(4000));
        });
    }

    // Serve in the background so the console stays interactive for sending.
    let app = Router::new()
        .route("/upload", post(upload))
        .route("/upload-multipart", post(upload_multipart))
        .route("/health", get(health))
        .route("/wizard", get(|| async { Html(WIZARD_HTML.to_string()) }))
        .route("/api/wizard/state", get(wizard_state))
        .route("/api/wizard/test-tunnel", post(wizard_test_tunnel))
        .route("/api/wizard/finish", post(wizard_finish))
        .route("/api/group/create", post(api_group_create))
        .route("/api/group/join", post(api_group_join))
        .route("/api/group/join-status", get(api_group_join_status))
        .route("/api/group/leave", post(api_group_leave))
        .route("/api/group/query", post(api_group_query))
        .route("/api/tunnel/status", get(api_tunnel_status))
        .route("/api/pairing/qr", get(pairing_qr))
        .route("/api/pairing/apply", post(pairing_apply))
        .route("/api/devices/refresh", get(refresh_devices))
        .route("/api/activity", get(activity_log))
        .layer(DefaultBodyLimit::disable());
    #[cfg(windows)]
    let _tray = setup_tray(port);
    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[server] FATAL: {e} — exiting in 5s");
            std::thread::sleep(std::time::Duration::from_secs(5));
            std::process::exit(1);
        }
    });

    // Release builds are a tray application on Windows, not an interactive
    // console program. Keep the HTTP receiver, tray and edge drop zone alive.
    #[cfg(windows)]
    {
        let _ = server.await;
        return;
    }

    // ---- interactive send loop ----
    use std::io::Write as _;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let mut peer_line = String::new();
    let mut path_line = String::new();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        line.clear();
        match stdin.read_line(&mut line).await {
            Ok(0) => break, // stdin closed (e.g. piped input) — keep receiving
            Ok(_) => {}
            Err(_) => break,
        }
        match line.trim() {
            "list" | "l" => {
                let peers = mdns::list();
                if peers.is_empty() {
                    println!("  no LAN devices discovered yet…");
                }
                for (i, p) in peers.iter().enumerate() {
                    println!("  {}. {} -> {}:{}", i + 1, p.name, p.ip, p.port);
                }
                let tps = tunnel::TUNNEL_PEERS.lock().unwrap().clone();
                for t in &tps {
                    println!("  🌐 {t} (tunnel)");
                }
            }
            "send" | "s" => {
                let peers = mdns::list();
                if peers.is_empty() {
                    println!("  no devices discovered yet — you can still type an IP");
                }
                for (i, p) in peers.iter().enumerate() {
                    println!("  {}. {} -> {}:{}", i + 1, p.name, p.ip, p.port);
                }
                print!("  peer (number / name / ip): ");
                let _ = std::io::stdout().flush();
                peer_line.clear();
                if stdin.read_line(&mut peer_line).await.unwrap_or(0) == 0 {
                    break;
                }
                let peer_input = peer_line.trim().to_string();
                let peer = match sender::resolve(&peer_input).await {
                    Ok(p) => p,
                    Err(e) => {
                        println!("  ✗ {e}");
                        continue;
                    }
                };
                print!("  file path: ");
                let _ = std::io::stdout().flush();
                path_line.clear();
                if stdin.read_line(&mut path_line).await.unwrap_or(0) == 0 {
                    break;
                }
                let path = path_line.trim().trim_matches('"').to_string();
                println!("  → sending to {}:{}…", peer.ip, peer.port);
                match sender::send_file(&peer.ip, peer.port, &path).await {
                    Ok(n) => println!("\r  ✓ sent {n} bytes to {}        ", peer.name),
                    Err(e) => println!("\r  ✗ failed: {e}"),
                }
            }
            "quit" | "q" | "exit" => {
                println!("bye — incoming files will no longer be accepted");
                std::process::exit(0);
            }
            "" => {}
            other => println!("  unknown: {other} — try: list | send | quit"),
        }
    }

    println!("input closed — continuing to receive files…");
    let _ = server.await;
}
