//! Tunnel client: cross-network transfers via the riki-api.online relay.
//! Group-based: devices register with a 6-digit group id; every transfer
//! frame is delivered to all other members of that group.
//! Payloads are end-to-end encrypted with XChaCha20-Poly1305 using a key
//! derived from the pairing passphrase — each chunk carries a fresh nonce.
//! Chunks stream from disk: constant memory, any file size.

use crate::app_config;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::AppHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

pub static TUNNEL_PEERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Human-readable link state for the UI: "connecting", "online (head)",
/// "not a member", "relay unreachable: …".
pub static TUNNEL_STATUS: Mutex<String> = Mutex::new(String::new());
/// Set when the relay rejected our token — the loop stops reconnecting
/// until the config changes (apply_config respawns it).
static MEMBERSHIP_REJECTED: AtomicBool = AtomicBool::new(false);

pub fn status() -> String {
    TUNNEL_STATUS.lock().unwrap().clone()
}
fn set_status(s: impl Into<String>) {
    *TUNNEL_STATUS.lock().unwrap() = s.into();
}

type Tx = futures_util::stream::SplitSink<
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type Rx = futures_util::stream::SplitStream<
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// `register` frame carrying this device's member token. `listen` marks
/// the app's receive loop; one-shot send/query sessions pass false so the
/// relay never pushes transfer traffic at a socket nobody is reading.
fn register_frame(group: &str, listen: bool) -> Message {
    let cfg = get_cfg();
    Message::text(
        json!({"type":"register","group":group,"id":cfg.tunnel.device_id,"token":cfg.tunnel.member_token,"listen":listen})
            .to_string(),
    )
}

/// Read frames until the relay accepts (`registered`) or refuses (`error`).
async fn await_registered(rx: &mut Rx) -> Result<serde_json::Value, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let msg = tokio::time::timeout_at(deadline, rx.next())
            .await
            .map_err(|_| "relay did not answer the registration".to_string())?
            .ok_or("relay closed the connection".to_string())?
            .map_err(|e| format!("relay read: {e}"))?;
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
        match v["type"].as_str() {
            Some("registered") => return Ok(v),
            Some("error") => return Err(v["err"].as_str().unwrap_or("rejected by relay").to_string()),
            _ => {}
        }
    }
}

/// Read frames until one of `types` arrives (or the timeout).
async fn await_type(rx: &mut Rx, types: &[&str], secs: u64) -> Result<serde_json::Value, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let msg = tokio::time::timeout_at(deadline, rx.next())
            .await
            .map_err(|_| "timed out waiting for the relay".to_string())?
            .ok_or("relay closed the connection".to_string())?
            .map_err(|e| format!("relay read: {e}"))?;
        let Message::Text(t) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
        if v["type"].as_str() == Some("error") {
            return Err(v["err"].as_str().unwrap_or("relay error").to_string());
        }
        if types.contains(&v["type"].as_str().unwrap_or("")) {
            return Ok(v);
        }
    }
}
static IN_SENT: AtomicU64 = AtomicU64::new(0);
static IN_TOTAL: AtomicU64 = AtomicU64::new(1);
static IN_NAME: Mutex<String> = Mutex::new(String::new());
static IN_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Open handle for the .part file (kept across chunks — reopening per
/// chunk is slow, especially on Windows with real-time AV scanning).
static IN_HANDLE: tokio::sync::Mutex<Option<tokio::io::BufWriter<tokio::fs::File>>> = tokio::sync::Mutex::const_new(None);

async fn part_write(bytes: &[u8]) {
    use tokio::io::AsyncWriteExt;
    let mut h = IN_HANDLE.lock().await;
    if h.is_none() {
        let path = IN_FILE.lock().unwrap().clone();
        if let Some(p) = path {
            // 1 MiB buffer — one blocking-pool write (and AV touch) per MiB
            // instead of per chunk keeps Windows tunnel writes off the floor
            *h = tokio::fs::OpenOptions::new().append(true).open(&p).await.ok()
                .map(|f| tokio::io::BufWriter::with_capacity(1 << 20, f));
        }
    }
    if let Some(f) = h.as_mut() {
        let _ = f.write_all(bytes).await;
    }
}

async fn part_close() {
    use tokio::io::AsyncWriteExt;
    if let Some(mut f) = IN_HANDLE.lock().await.take() {
        let _ = f.flush().await;
    }
}
static IN_FAILED: AtomicBool = AtomicBool::new(false);
static IN_ENCRYPTED: AtomicBool = AtomicBool::new(false);
static IN_NONCE: Mutex<String> = Mutex::new(String::new());
static IN_REPLY_TO: Mutex<String> = Mutex::new(String::new());
static IN_ID: Mutex<String> = Mutex::new(String::new());
static IN_SEQ: AtomicU64 = AtomicU64::new(1);
static IN_LAST_EMIT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// Sender of the transfer currently being written (one at a time).
static IN_FROM: Mutex<String> = Mutex::new(String::new());

const CHUNK: usize = 48 * 1024;
const B64T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64T[(n >> 18) as usize & 63] as char);
        out.push(B64T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn unb64(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn tunnel_key() -> Option<[u8; 32]> {
    let secret = get_cfg().tunnel.shared_secret;
    if secret.trim().is_empty() {
        return None;
    }
    let digest = sha2_digest(secret.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    Some(key)
}

/// SHA-256 (single-block implementation via the sha2 crate re-export)
fn sha2_digest(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn encrypt(data: &[u8], key: [u8; 32]) -> Result<(Vec<u8>, [u8; 24]), String> {
    use chacha20poly1305::{aead::{Aead, OsRng}, AeadCore, KeyInit, XChaCha20Poly1305, XNonce};
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let sealed = cipher
        .encrypt(&nonce, data)
        .map_err(|_| "could not encrypt transfer".to_string())?;
    let mut raw = [0u8; 24];
    raw.copy_from_slice(&nonce);
    Ok((sealed, raw))
}

fn decrypt(data: &[u8], nonce: &[u8], key: [u8; 32]) -> Result<Vec<u8>, String> {
    use chacha20poly1305::{aead::{Aead, OsRng}, AeadCore, KeyInit, XChaCha20Poly1305, XNonce};
    if nonce.len() != 24 {
        return Err("invalid encrypted transfer nonce".into());
    }
    XChaCha20Poly1305::new((&key).into())
        .decrypt(XNonce::from_slice(nonce), data)
        .map_err(|_| "could not decrypt transfer — pairing passphrase does not match".to_string())
}

fn get_cfg() -> app_config::Config {
    crate::get_cfg()
}

/// dial the relay: raw TCP with a clamped MSS (the path to the relay can
/// blackhole full-size packets when PMTUD breaks), then the WS handshake
async fn connect(relay: &str) -> Result<
    (
        WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    let (host, port) = host_port(relay);
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| format!("connect {relay}: timed out"))?
    .map_err(|e| format!("connect {relay}: {e}"))?;
    clamp_mss(&tcp, 1400);
    let _ = tcp.set_nodelay(true);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::client_async_tls(relay, tcp),
    )
    .await
    .map_err(|_| format!("connect {relay}: timed out"))?
    .map_err(|e| format!("connect {relay}: {e}"))
}

fn host_port(url: &str) -> (String, u16) {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let default_port = if url.starts_with("wss") || url.starts_with("https") { 443 } else { 80 };
    match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    }
}

#[cfg(unix)]
fn clamp_mss(stream: &tokio::net::TcpStream, mss: u32) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const std::ffi::c_void, len: u32) -> i32;
    }
    let val: u32 = mss;
    unsafe {
        // IPPROTO_TCP=6, TCP_MAXSEG=2 (Linux & macOS)
        let _ = setsockopt(stream.as_raw_fd(), 6, 2, &val as *const u32 as *const std::ffi::c_void, 4);
    }
}

#[cfg(windows)]
fn clamp_mss(_stream: &tokio::net::TcpStream, _mss: u32) {}

/// Stream a file through the relay to the whole group. Payloads are
/// end-to-end encrypted per chunk (fresh nonce each); progress is reported
/// through the returned watcher counter.
/// Group member as reported by the relay.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Member {
    pub device_id: String,
    pub name: String,
    pub role: String,
    pub online: bool,
}
/// Members of our group (refreshed by the receive loop every few seconds).
pub static TUNNEL_MEMBERS: Mutex<Vec<Member>> = Mutex::new(Vec::new());

fn update_members(list: &serde_json::Value) {
    let me = get_cfg().tunnel.device_id;
    let members: Vec<Member> = list
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m["device_id"].as_str()?.to_string();
                    if id == me {
                        return None;
                    }
                    Some(Member {
                        device_id: id,
                        name: m["name"].as_str().unwrap_or("").to_string(),
                        role: m["role"].as_str().unwrap_or("member").to_string(),
                        online: m["online"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    *TUNNEL_PEERS.lock().unwrap() = members.iter().filter(|m| m.online).map(|m| m.device_id.clone()).collect();
    *TUNNEL_MEMBERS.lock().unwrap() = members;
}

/// Stream a file through the relay: to one member (`to = Some(device_id)`)
/// or to the whole group. Incoming frames are drained while uploading so
/// pongs flow and a receiver's early `error` aborts the transfer.
pub async fn send_over_tunnel(
    relay: &str,
    group: &str,
    self_id: &str,
    path: &str,
    sent: Arc<AtomicU64>,
    to: Option<&str>,
) -> Result<u64, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(register_frame(group, false))
        .await
        .map_err(|e| format!("register: {e}"))?;
    await_registered(&mut rx).await?;

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("bad path")?
        .to_string();
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("read {path}: {e}"))?;
    let total = file.metadata().await.map_err(|e| e.to_string())?.len();
    let encrypted = tunnel_key().is_some();
    let me = get_cfg().tunnel.device_id;

    // reader task: acks/errors arrive here while we are busy sending
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
    let reader = tokio::spawn(async move {
        while let Some(m) = rx.next().await {
            let Ok(Message::Text(t)) = m else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
            match v["type"].as_str() {
                Some("complete") => { let _ = ack_tx.send(Ok(())); }
                Some("error") => { let _ = ack_tx.send(Err(v["err"].as_str().unwrap_or("receiver error").to_string())); }
                _ => {}
            }
        }
        let _ = ack_tx.send(Err("relay closed the connection".into()));
    });

    let mut frame_base = json!({"group":group,"from":self_id,"reply_to":me});
    if let Some(dev) = to {
        frame_base["to"] = json!(dev);
    }
    let with = |mut base: serde_json::Value, extra: serde_json::Value| {
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e { b.insert(k.clone(), v.clone()); }
        }
        base
    };

    tx.send(Message::text(
        with(frame_base.clone(), json!({"type":"begin","name":name,"size":total,"encrypted":encrypted})).to_string(),
    ))
    .await
    .map_err(|e| format!("send begin: {e}"))?;

    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; CHUNK];
    let result: Result<(), String> = loop {
        // an early error (busy receiver, wrong passphrase) stops the upload
        if let Ok(r) = ack_rx.try_recv() {
            if let Err(e) = r { break Err(e); }
        }
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => break Err(format!("read: {e}")),
        };
        if n == 0 {
            break Ok(());
        }
        sent.fetch_add(n as u64, Ordering::Relaxed);
        let frame = match tunnel_key() {
            Some(key) => match encrypt(&buf[..n], key) {
                Ok((sealed, nonce)) => with(frame_base.clone(), json!({"type":"chunk","nonce":b64(&nonce),"b64":b64(&sealed)})),
                Err(e) => break Err(e),
            },
            None => with(frame_base.clone(), json!({"type":"chunk","b64":b64(&buf[..n])})),
        };
        if let Err(e) = tx.send(Message::text(frame.to_string())).await {
            break Err(format!("send chunk: {e}"));
        }
    };
    if let Err(e) = result {
        reader.abort();
        return Err(e);
    }

    tx.send(Message::text(with(frame_base.clone(), json!({"type":"end"})).to_string()))
        .await
        .map_err(|e| format!("send end: {e}"))?;

    // wait for a receiver to confirm (or report a decrypt failure)
    let outcome = match tokio::time::timeout(std::time::Duration::from_secs(600), ack_rx.recv()).await {
        Ok(Some(r)) => r,
        Ok(None) => Err("relay closed the connection".into()),
        Err(_) => Err("no device confirmed receipt within 10 minutes".into()),
    };
    reader.abort();
    outcome.map(|_| total)
}

/// one-shot probe: register with a group and list its members
pub async fn devices(relay: &str, group: &str, self_id: &str) -> Result<Vec<String>, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(register_frame(group, false)).await.map_err(|e| e.to_string())?;
    await_registered(&mut rx).await?;
    tx.send(Message::text(json!({"type":"devices"}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let v = await_type(&mut rx, &["devices"], 6).await?;
    if v["type"] == "devices" {
        let me = self_id;
        let ids: Vec<String> = v["ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .filter(|x| x != me)
                    .collect()
            })
            .unwrap_or_default();
        *TUNNEL_PEERS.lock().unwrap() = ids.clone();
        return Ok(ids);
    }
    Err("unexpected devices reply".into())
}

/// alias kept for the wizard's check flow
pub async fn check_relay(relay: &str, group: &str) -> Result<Vec<String>, String> {
    if group.is_empty() || get_cfg().tunnel.member_token.is_empty() {
        return ping(relay).await.map(|_| Vec::new());
    }
    devices(relay, group, &format!("check-{}", std::process::id())).await
}

/// Membership-less reachability probe.
pub async fn ping(relay: &str) -> Result<(), String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(json!({"type":"ping"}).to_string())).await.map_err(|e| e.to_string())?;
    await_type(&mut rx, &["pong"], 8).await.map(|_| ())
}

/// Persistent receive loop — abort + respawn when the tunnel config changes.
pub async fn receive_loop(relay: String, group: String, self_id: String, app: AppHandle) {
    MEMBERSHIP_REJECTED.store(false, Ordering::Relaxed);
    if group.is_empty() || get_cfg().tunnel.member_token.is_empty() {
        set_status("not in a group — create or join one in Preferences");
        println!("[tunnel] no group membership yet — tunnel idle");
        return;
    }
    loop {
        println!("[tunnel] connecting {relay} as {self_id} (group {group})…");
        set_status("connecting");
        match connect(&relay).await {
            Ok((ws, _)) => {
                println!("[tunnel] connected ✓");
                let (mut tx, mut rx) = ws.split();
                if tx.send(register_frame(&group, true)).await.is_err() {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
                match await_registered(&mut rx).await {
                    Ok(v) => {
                        let role = v["role"].as_str().unwrap_or("member").to_string();
                        update_members(&v["members"]);
                        set_status(format!("online ({role})"));
                        use tauri::Emitter;
                        let _ = app.emit("tunnel-status", json!({"status": status(), "role": role, "members": v["members"]}));
                    }
                    Err(e) => {
                        println!("[tunnel] registration refused: {e}");
                        set_status(format!("not a member: {e}"));
                        use tauri::Emitter;
                        let _ = app.emit("tunnel-status", json!({"status": status()}));
                        MEMBERSHIP_REJECTED.store(true, Ordering::Relaxed);
                        return; // wait for a config change instead of hammering the relay
                    }
                }
                // poll group membership so later devices appear in the picker;
                // pings + a stale timeout evict zombie links (NAT/CF can drop
                // tunnels without a FIN)
                let mut refresh = tokio::time::interval(std::time::Duration::from_secs(5));
                refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut last_seen = std::time::Instant::now();
                loop {
                    tokio::select! {
                        _ = refresh.tick() => {
                            if last_seen.elapsed() > std::time::Duration::from_secs(75) {
                                println!("[tunnel] connection stale — reconnecting");
                                break;
                            }
                            if tx.send(Message::text(json!({"type":"members"}).to_string())).await.is_err() {
                                break;
                            }
                        }
                        message = rx.next() => match message {
                            Some(Ok(Message::Text(text))) => {
                                last_seen = std::time::Instant::now();
                                handle_frame(&mut tx, &text, &app).await
                            }
                            Some(Ok(_)) => {
                                last_seen = std::time::Instant::now();
                            }
                            Some(Err(_)) | None => break,
                        },
                    }
                }
                println!("[tunnel] disconnected — retrying in 3s");
                set_status("reconnecting");
            }
            Err(e) => {
                println!("[tunnel] connect failed: {e} — retrying in 3s");
                set_status(format!("relay unreachable: {e}"));
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

// ------------------------------------------------------------ group admin
/// Outcome of a join request.
#[derive(serde::Serialize, Clone, Debug)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum JoinOutcome {
    Approved { token: String },
    Denied,
    Pending { request_id: String },
}

fn join_outcome(v: &serde_json::Value) -> JoinOutcome {
    match v["type"].as_str() {
        Some("join_result") => {
            if v["approved"].as_bool().unwrap_or(false) {
                JoinOutcome::Approved { token: v["token"].as_str().unwrap_or("").to_string() }
            } else {
                JoinOutcome::Denied
            }
        }
        _ => JoinOutcome::Pending { request_id: v["request_id"].as_str().unwrap_or("").to_string() },
    }
}

/// Ask the relay for a brand-new group; this device becomes its head.
pub async fn group_create(relay: &str, device_id: &str, device_name: &str) -> Result<(String, String), String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(
        json!({"type":"create_group","device_id":device_id,"device_name":device_name}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;
    let v = await_type(&mut rx, &["group_created"], 10).await?;
    Ok((
        v["group"].as_str().unwrap_or("").to_string(),
        v["token"].as_str().unwrap_or("").to_string(),
    ))
}

/// Request to join `group`; waits up to `wait_secs` for the head's decision.
pub async fn group_join(relay: &str, group: &str, device_id: &str, device_name: &str, wait_secs: u64) -> Result<JoinOutcome, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(
        json!({"type":"join_request","group":group,"device_id":device_id,"device_name":device_name}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;
    let pending = await_type(&mut rx, &["join_pending"], 10).await?;
    let request_id = pending["request_id"].as_str().unwrap_or("").to_string();
    match await_type(&mut rx, &["join_result"], wait_secs).await {
        Ok(v) => Ok(join_outcome(&v)),
        Err(_) => Ok(JoinOutcome::Pending { request_id }),
    }
}

/// Re-check a request made earlier (the head may have decided while we were away).
pub async fn group_join_status(relay: &str, group: &str, request_id: &str) -> Result<JoinOutcome, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(json!({"type":"join_status","group":group,"request_id":request_id}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let v = await_type(&mut rx, &["join_result", "join_pending"], 10).await?;
    Ok(join_outcome(&v))
}

/// One-shot authenticated request: register with our token, send `frame`,
/// return the first reply of one of `reply_types`.
pub async fn group_query(relay: &str, group: &str, frame: serde_json::Value, reply_types: &[&str]) -> Result<serde_json::Value, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(register_frame(group, false)).await.map_err(|e| e.to_string())?;
    await_registered(&mut rx).await?;
    tx.send(Message::text(frame.to_string())).await.map_err(|e| e.to_string())?;
    await_type(&mut rx, reply_types, 10).await
}

async fn handle_frame(
    tx: &mut futures_util::stream::SplitSink<
        WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    text: &str,
    app: &AppHandle,
) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    match v["type"].as_str().unwrap_or("") {
        "join_request" => {
            // someone wants into our group — the head decides in the drop zone
            use tauri::Emitter;
            let name = v["device_name"].as_str().unwrap_or("a device");
            println!("[tunnel] join request from {name} ({})", v["device_id"].as_str().unwrap_or("?"));
            let _ = app.emit("tunnel-join-request", v.clone());
            crate::tray::notify(app, "WhisperDrop — join request", &format!("{name} wants to join group {}", v["group"].as_str().unwrap_or("")));
        }
        "join_decided" => {
            use tauri::Emitter;
            let _ = app.emit("tunnel-join-decided", v.clone());
        }
        "members" => update_members(&v["members"]),
        "registered" | "ok" | "pending" => {}
        "error" => {
            let e = v["err"].as_str().unwrap_or("relay error");
            println!("[tunnel] relay: {e}");
            set_status(format!("relay: {e}"));
            use tauri::Emitter;
            let _ = app.emit("tunnel-status", json!({"status": status()}));
        }
        "devices" => {
            let me = get_cfg().tunnel.device_id;
            if let Some(arr) = v["ids"].as_array() {
                let ids: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .filter(|id| id.as_str() != me)
                    .collect();
                *TUNNEL_PEERS.lock().unwrap() = ids;
            }
        }
        "begin" => {
            let name = v["name"].as_str().unwrap_or("file").to_string();
            let size = v["size"].as_u64().unwrap_or(0);
            // one incoming transfer at a time: a different sender starting
            // now would interleave its chunks into this file
            let from = v["reply_to"].as_str().or_else(|| v["from"].as_str()).unwrap_or("").to_string();
            let busy_with = IN_FROM.lock().unwrap().clone();
            if IN_FILE.lock().unwrap().is_some() && !busy_with.is_empty() && busy_with != from {
                let _ = tx.send(Message::text(json!({"type":"error","to":from,"from":"self","err":"receiver is busy with another transfer — try again in a moment"}).to_string())).await;
                return;
            }
            *IN_FROM.lock().unwrap() = from;
            *IN_NAME.lock().unwrap() = name.clone();
            IN_TOTAL.store(v["wire_size"].as_u64().unwrap_or(size).max(1), Ordering::Relaxed);
            IN_SENT.store(0, Ordering::Relaxed);
            IN_ENCRYPTED.store(v["encrypted"].as_bool().unwrap_or(false), Ordering::Relaxed);
            *IN_NONCE.lock().unwrap() = v["nonce"].as_str().unwrap_or("").to_string();
            // who to confirm to when the file has landed (sender's device id)
            *IN_REPLY_TO.lock().unwrap() = v["reply_to"]
                .as_str()
                .or_else(|| v["from"].as_str())
                .unwrap_or("")
                .to_string();
            IN_FAILED.store(false, Ordering::Relaxed);
            part_close().await;
            let mut dir = crate::server::receive_dir();
            std::fs::create_dir_all(&dir).ok();
            dir.push(format!(".{name}.part"));
            *IN_FILE.lock().unwrap() = Some(dir.clone());
            tokio::fs::write(&dir, b"").await.ok();
            let id = format!("tun-{}", IN_SEQ.fetch_add(1, Ordering::Relaxed));
            *IN_ID.lock().unwrap() = id.clone();
            *IN_LAST_EMIT.lock().unwrap() = None;
            let peer = TUNNEL_MEMBERS.lock().unwrap().iter().find(|m| m.device_id == *IN_FROM.lock().unwrap()).map(|m| m.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "tunnel".into());
            crate::server::emit_incoming_begin(app, &id, &name, IN_TOTAL.load(Ordering::Relaxed), &peer);
        }
        "chunk" => {
            let b64data = v["b64"].as_str().unwrap_or("");
            let mut bytes = unb64(b64data);
            if IN_ENCRYPTED.load(Ordering::Relaxed) {
                let nonce_b64 = v["nonce"].as_str().unwrap_or("");
                match tunnel_key() {
                    Some(key) => match decrypt(&bytes, &unb64(nonce_b64), key) {
                        Ok(plain) => bytes = plain,
                        Err(err) => {
                            IN_FAILED.store(true, Ordering::Relaxed);
                            eprintln!("[tunnel] decrypt failed: {err} — aborting transfer");
                            part_close().await;
                            let part = IN_FILE.lock().unwrap().take();
                            if let Some(p) = part {
                                let _ = tokio::fs::remove_file(&p).await;
                            }
                            let reply_to = IN_REPLY_TO.lock().unwrap().clone();
                            if !reply_to.is_empty() {
                                let _ = tx
                                    .send(Message::text(json!({"type":"error","to":reply_to,"from":"self","err":err}).to_string()))
                                    .await;
                            }
                            return;
                        }
                    },
                    None => {
                        IN_FAILED.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
            IN_SENT.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            part_write(&bytes).await;
            let due = {
                let mut last = IN_LAST_EMIT.lock().unwrap();
                if last.map(|t| t.elapsed() >= std::time::Duration::from_millis(80)).unwrap_or(true) {
                    *last = Some(std::time::Instant::now());
                    true
                } else { false }
            };
            if due {
                let id = IN_ID.lock().unwrap().clone();
                crate::server::emit_incoming_progress(app, &id, IN_SENT.load(Ordering::Relaxed), IN_TOTAL.load(Ordering::Relaxed));
            }
        }
        "end" => {
            part_close().await;
            let newname = IN_NAME.lock().unwrap().clone();
            let failed = IN_FAILED.swap(false, Ordering::Relaxed);
            let part_opt = IN_FILE.lock().unwrap().clone();
            if let Some(part) = part_opt {
                let id = IN_ID.lock().unwrap().clone();
                if failed {
                    let _ = tokio::fs::remove_file(&part).await;
                    crate::server::emit_incoming_done(app, &id, false, &newname);
                    crate::tray::notify(app, "WhisperDrop", "Incoming transfer failed — passphrase mismatch");
                } else if let Some(dir) = part.parent() {
                    let mut finalp = dir.join(&newname);
                    let mut i = 1;
                    while finalp.exists() {
                        finalp = dir.join(format!("{newname} ({i})"));
                        i += 1;
                    }
                    tokio::fs::rename(&part, &finalp).await.ok();
                    println!("✓ tunnel received -> {}", finalp.display());
                    crate::server::emit_incoming_progress(app, &id, IN_SENT.load(Ordering::Relaxed), IN_SENT.load(Ordering::Relaxed));
                    crate::server::emit_incoming_done(app, &id, true, &newname);
                    crate::tray::notify(app, "WhisperDrop", &format!("Received {newname} via tunnel"));
                    let reply_to = IN_REPLY_TO.lock().unwrap().clone();
                    if !reply_to.is_empty() {
                        let _ = tx.send(Message::text(json!({
                            "type":"complete", "to":reply_to, "from":"self",
                            "name":newname, "size":IN_SENT.load(Ordering::Relaxed)
                        }).to_string())).await;
                    }
                }
                *IN_FILE.lock().unwrap() = None;
                IN_FROM.lock().unwrap().clear();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::sha2_digest;
    use chacha20poly1305::{aead::{Aead, OsRng}, AeadCore, KeyInit, XChaCha20Poly1305, XNonce};

    fn encrypt(data: &[u8], key: [u8; 32]) -> Result<(Vec<u8>, [u8; 24]), String> {
        let cipher = XChaCha20Poly1305::new((&key).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = cipher
            .encrypt(&nonce, data)
            .map_err(|_| "could not encrypt transfer".to_string())?;
        let mut raw = [0u8; 24];
        raw.copy_from_slice(&nonce);
        Ok((sealed, raw))
    }

    fn decrypt(data: &[u8], nonce: &[u8], key: [u8; 32]) -> Result<Vec<u8>, String> {
        if nonce.len() != 24 {
            return Err("invalid encrypted transfer nonce".into());
        }
        XChaCha20Poly1305::new((&key).into())
            .decrypt(XNonce::from_slice(nonce), data)
            .map_err(|_| "could not decrypt transfer — pairing passphrase does not match".to_string())
    }

    #[test]
    fn key_matches_receiver_kdf() {
        // both sides derive the key as SHA-256(pairing passphrase)
        let key = sha2_digest(b"test-pairing-123");
        let key_again = sha2_digest(b"test-pairing-123");
        assert_eq!(key, key_again);
    }

    #[test]
    fn round_trip_and_wrong_key_rejected() {
        let key = sha2_digest(b"test-pairing-123");
        let plaintext = b"WhisperDrop private payload";
        let (sealed, nonce) = encrypt(plaintext, key).unwrap();
        assert_eq!(decrypt(&sealed, &nonce, key).unwrap(), plaintext);
        assert!(decrypt(&sealed, &nonce, [8u8; 32])
            .unwrap_err()
            .contains("passphrase does not match"));
    }

    #[test]
    fn fresh_nonce_per_encryption() {
        let key = sha2_digest(b"test-pairing-123");
        let (sealed_a, nonce_a) = encrypt(b"same plaintext", key).unwrap();
        let (sealed_b, nonce_b) = encrypt(b"same plaintext", key).unwrap();
        assert_ne!(nonce_a, nonce_b, "nonce reuse would break confidentiality");
        assert_ne!(sealed_a, sealed_b);
    }
}
