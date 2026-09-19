//! Tunnel: transfer across networks via a relay on riki-api.online.
//! Protocol (JSON text frames over WebSocket):
//!   -> {"type":"register","id":"<device_id>"}              (first message)
//!   -> {"type":"devices"}                                  / <- {"type":"devices","ids":[...]}
//!   -> {"type":"begin","to":id,"from":id,"name":n,"size":s}
//!   -> {"type":"chunk","to":id,"from":id,"b64":"..."}      (≤48 KB raw)
//!   -> {"type":"end","to":id,"from":id}
//!   <- same frames routed by the relay; "from" = sender device id

use crate::overlay;
use chacha20poly1305::{
    aead::{Aead, OsRng},
    AeadCore, KeyInit, XChaCha20Poly1305, XNonce,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

pub static TUNNEL_PEERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Link state for the UI/CLI: "connecting", "online (head)", "not a member: …".
pub static TUNNEL_STATUS: Mutex<String> = Mutex::new(String::new());
pub fn status() -> String {
    TUNNEL_STATUS.lock().unwrap().clone()
}
fn set_status(s: impl Into<String>) {
    *TUNNEL_STATUS.lock().unwrap() = s.into();
}

type Tx = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type Rx = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

fn cfg_snapshot() -> crate::config::Config {
    crate::RUNTIME_CFG.lock().unwrap().clone().unwrap_or_else(crate::config::load)
}

/// `register` frame carrying this device's id and member token.
/// `listen` = this session receives transfer traffic (the app's receive
/// loop); one-shot send/query sessions pass false so the relay never
/// pushes other people's chunks at a socket nobody is reading.
fn register_frame(group: &str, listen: bool) -> Message {
    let cfg = cfg_snapshot();
    Message::text(
        json!({"type":"register","group":group,"id":cfg.tunnel.device_id,"token":cfg.tunnel.member_token,"listen":listen}).to_string(),
    )
}

/// Read frames until the relay accepts (`registered`) or refuses (`error`).
async fn await_registered(rx: &mut Rx) -> Result<serde_json::Value, String> {
    await_type(rx, &["registered"], 10).await
}

/// Read frames until one of `types` arrives (or the timeout). `error` frames fail.
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

// ------------------------------------------------------------ group admin
#[derive(Clone, Debug, serde::Serialize)]
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
    tx.send(Message::text(json!({"type":"create_group","device_id":device_id,"device_name":device_name}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let v = await_type(&mut rx, &["group_created"], 10).await?;
    Ok((v["group"].as_str().unwrap_or("").to_string(), v["token"].as_str().unwrap_or("").to_string()))
}

/// Request to join `group`; waits up to `wait_secs` for the head's decision.
pub async fn group_join(relay: &str, group: &str, device_id: &str, device_name: &str, wait_secs: u64) -> Result<JoinOutcome, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(json!({"type":"join_request","group":group,"device_id":device_id,"device_name":device_name}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let pending = await_type(&mut rx, &["join_pending"], 10).await?;
    let request_id = pending["request_id"].as_str().unwrap_or("").to_string();
    match await_type(&mut rx, &["join_result"], wait_secs).await {
        Ok(v) => Ok(join_outcome(&v)),
        Err(_) => Ok(JoinOutcome::Pending { request_id }),
    }
}

/// Re-check a request made earlier.
pub async fn group_join_status(relay: &str, group: &str, request_id: &str) -> Result<JoinOutcome, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(json!({"type":"join_status","group":group,"request_id":request_id}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let v = await_type(&mut rx, &["join_result", "join_pending"], 10).await?;
    Ok(join_outcome(&v))
}

/// One-shot authenticated request: register, send `frame`, return the reply.
pub async fn group_query(relay: &str, group: &str, frame: serde_json::Value, reply_types: &[&str]) -> Result<serde_json::Value, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(register_frame(group, false)).await.map_err(|e| e.to_string())?;
    await_registered(&mut rx).await?;
    tx.send(Message::text(frame.to_string())).await.map_err(|e| e.to_string())?;
    await_type(&mut rx, reply_types, 10).await
}

/// A join request arrived (this device is the head): ask the user.
/// Windows shows a native Yes/No dialog; elsewhere the CLI is the way.
fn on_join_request(v: &serde_json::Value) {
    let name = v["device_name"].as_str().unwrap_or("a device").to_string();
    let dev = v["device_id"].as_str().unwrap_or("?").to_string();
    let group = v["group"].as_str().unwrap_or("").to_string();
    let rid = v["request_id"].as_str().unwrap_or("").to_string();
    println!("[tunnel] join request: {name} ({dev}) wants to join group {group} — approve with: whisperdrop group approve {rid}");
    crate::activity::write(format!("join request from {name} ({dev}) — id {rid}"));
    #[cfg(windows)]
    {
        let (relay, cfg_group) = {
            let c = cfg_snapshot();
            (c.tunnel.relay.clone(), c.group_id.clone())
        };
        let Some(handle) = crate::rt_handle() else { return };
        std::thread::spawn(move || {
            let text = format!(
                "\"{name}\" (device {dev}) wants to join your WhisperDrop group {group}.\n\nApproving lets it see your group and exchange files with you.\n\nApprove?"
            );
            let yes = crate::win_confirm("WhisperDrop — join request", &text);
            handle.spawn(async move {
                let r = group_query(&relay, &cfg_group, json!({"type":"approve","request_id":rid,"approved":yes}), &["ok"]).await;
                println!("[tunnel] {} {name}: {:?}", if yes { "approved" } else { "denied" }, r.map(|_| ()));
            });
        });
    }
}
static SENT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn sent_count() -> u64 {
    SENT_COUNTER.load(std::sync::atomic::Ordering::Relaxed)
}
fn sent_reset() {
    SENT_COUNTER.store(0, std::sync::atomic::Ordering::Relaxed);
}
fn sent_add(n: u64) {
    SENT_COUNTER.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}
static CURRENT_GROUP: Mutex<String> = Mutex::new(String::new());

pub fn set_group(group: String) {
    *CURRENT_GROUP.lock().unwrap() = group;
}

pub fn current_group() -> String {
    CURRENT_GROUP.lock().unwrap().clone()
}
// incoming transfer state
static IN_SENT: AtomicU64 = AtomicU64::new(0);
static IN_TOTAL: AtomicU64 = AtomicU64::new(1);
static IN_NAME: Mutex<String> = Mutex::new(String::new());
static IN_FILE: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
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
static IN_REPLY_TO: Mutex<String> = Mutex::new(String::new());
/// Sender of the transfer currently being written (one at a time).
static IN_FROM: Mutex<String> = Mutex::new(String::new());
static IN_ENCRYPTED: AtomicBool = AtomicBool::new(false);
static IN_FAILED: AtomicBool = AtomicBool::new(false);
static IN_NONCE: Mutex<String> = Mutex::new(String::new());
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

const B64T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

fn tunnel_key() -> Option<[u8; 32]> {
    let secret = crate::RUNTIME_CFG
        .lock()
        .unwrap()
        .as_ref()?
        .tunnel
        .shared_secret
        .clone();
    if secret.trim().is_empty() {
        return None;
    }
    let digest = Sha256::digest(secret.as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    Some(key)
}

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

async fn connect(
    relay: &str,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    // dial the TCP socket ourselves so we can clamp TCP_MAXSEG: the path to
    // the relay can blackhole full-size packets when PMTUD breaks (ICMP
    // "frag needed" suppressed), which stalls every transfer after the
    // handshake. 1400 survives PPPoE/tunnel overheads.
    let (host, port) = host_port(relay);
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| format!("connect {relay}: timed out"))?
    .map_err(|e| format!("connect {relay}: {e}"))?;
    // clamp disabled for testing
    let _ = tcp.set_nodelay(true);
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::client_async_tls(relay, tcp),
    )
    .await
    .map_err(|_| format!("connect {relay}: timed out"))?
    .map_err(|e| format!("connect {relay}: {e}"))
}

/// Verify the actual relay data path, not merely the TLS/WebSocket handshake.
/// A proxy can accept an upgrade while its relay worker is unavailable, so a
/// harmless frame is routed back to a temporary id before setup reports OK.
/// Is the relay usable? The relay is group-scoped and never echoes a frame
/// back to its sender, so a round trip is proven by registering in a
/// throwaway group and getting the relay's own `devices` reply.
pub async fn check_relay(relay: &str, self_id: &str) -> Result<(), String> {
    if current_group().is_empty() || cfg_snapshot().tunnel.member_token.is_empty() {
        return ping(relay).await;
    }
    devices(relay, self_id).await.map(|_| ())
}

/// Membership-less reachability probe.
pub async fn ping(relay: &str) -> Result<(), String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(json!({"type":"ping"}).to_string())).await.map_err(|e| e.to_string())?;
    await_type(&mut rx, &["pong"], 8).await.map(|_| ())
}

fn host_port(url: &str) -> (String, u16) {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split(['/']).next().unwrap_or(rest);
    let default_port = if url.starts_with("wss") || url.starts_with("https") {
        443
    } else {
        80
    };
    match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    }
}

#[cfg(unix)]
fn clamp_mss(stream: &tokio::net::TcpStream, mss: u32) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn setsockopt(
            fd: i32,
            level: i32,
            name: i32,
            value: *const std::ffi::c_void,
            len: u32,
        ) -> i32;
    }
    let val: u32 = mss;
    unsafe {
        // IPPROTO_TCP=6, TCP_MAXSEG=2 (Linux & macOS)
        let _ = setsockopt(
            stream.as_raw_fd(),
            6,
            2,
            &val as *const u32 as *const std::ffi::c_void,
            4,
        );
    }
}

#[cfg(windows)]
fn clamp_mss(_stream: &tokio::net::TcpStream, _mss: u32) {}

/// Short-lived requests must not register as the long-lived device identity:
/// an older relay removes an id when *any* socket for that id closes.
fn session_id(kind: &str, _device_id: &str) -> String {
    let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Keep transient registration IDs short for relay compatibility. They
    // only need to route the completion acknowledgement for this session.
    let prefix = kind.chars().next().unwrap_or('s');
    format!(
        "{prefix}{:06x}",
        (now as u64 ^ seq ^ std::process::id() as u64) & 0xFF_FFFF
    )
}

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
        out.push(if chunk.len() > 1 {
            B64T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64T[n as usize & 63] as char
        } else {
            '='
        });
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
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
        .collect();
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

/// Ask the relay which devices are online (one-shot connection).
pub async fn devices(relay: &str, self_id: &str) -> Result<Vec<String>, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    let request_id = self_id.to_string();
    let group = current_group();
    if group.is_empty() {
        return Err("not in a group — run `whisperdrop group create` or `whisperdrop group join <id>`".into());
    }
    tx.send(register_frame(&group, false)).await.map_err(|e| e.to_string())?;
    await_registered(&mut rx).await?;
    tx.send(Message::text(json!({"type":"devices"}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let v = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while let Some(message) = rx.next().await {
            let Message::Text(text) = message.map_err(|e| e.to_string())? else {
                continue;
            };
            let frame: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if frame["type"] == "devices" {
                return Ok(frame);
            }
        }
        Err("relay disconnected before replying to device scan".to_string())
    })
    .await
    .map_err(|_| "relay did not reply to device scan within 8 seconds".to_string())??;
    let ids: Vec<String> = v["ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .filter(|x| x != self_id && x != &request_id)
                .collect()
        })
        .unwrap_or_default();
    *TUNNEL_PEERS.lock().unwrap() = ids.clone();
    Ok(ids)
}

/// Send a file through the tunnel to a device id (one-shot connection).
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

pub fn members() -> Vec<Member> {
    TUNNEL_MEMBERS.lock().unwrap().clone()
}

fn update_members(list: &serde_json::Value) {
    let me = cfg_snapshot().tunnel.device_id;
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

/// Stream a file through the relay to one member (`to = Some(device_id)`)
/// or to the whole group.
pub async fn send_over_tunnel(
    relay: &str,
    group: &str,
    path: &str,
    to: Option<&str>,
) -> Result<u64, String> {
    match send_over_tunnel_once(relay, group, path, to).await {
        Ok(n) => Ok(n),
        // re-sending after the receiver merely hasn't confirmed yet would
        // collide with the transfer still being written on the other side
        Err(e) if e.contains("confirm") || e.contains("passphrase") || e.contains("not a member") || e.contains("busy") => Err(e),
        Err(e) => {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            send_over_tunnel_once(relay, group, path, to).await
                .map_err(|e2| format!("{e} | retry: {e2}"))
        }
    }
}

async fn send_over_tunnel_once(
    relay: &str,
    group: &str,
    path: &str,
    to: Option<&str>,
) -> Result<u64, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    let request_id = cfg_snapshot().tunnel.device_id;
    tx.send(register_frame(group, false)).await.map_err(|e| e.to_string())?;
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

    // reader task: acks/errors arrive here while we are busy sending, and
    // polling the stream keeps pongs flowing so the link stays healthy
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

    let mut base = json!({"group":group,"from":request_id,"reply_to":request_id});
    if let Some(dev) = to {
        base["to"] = json!(dev);
    }
    let with = |mut b: serde_json::Value, extra: serde_json::Value| {
        if let (Some(bo), Some(eo)) = (b.as_object_mut(), extra.as_object()) {
            for (k, v) in eo { bo.insert(k.clone(), v.clone()); }
        }
        b
    };

    tx.send(Message::text(
        with(base.clone(), json!({"type":"begin","name":name,"size":total,"wire_size":total,"encrypted":encrypted})).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;

    // stream the file: every chunk is encrypted with a FRESH random nonce
    // (no reuse, constant memory — multi-GB files never touch RAM)
    use tokio::io::AsyncReadExt;
    sent_reset();
    let ticker_total = total;
    let ticker = tokio::spawn(async move {
        let start = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let cur = crate::tunnel::sent_count();
            let pct = if ticker_total > 0 { cur * 100 / ticker_total } else { 100 };
            print!("\r  ↑ {cur} / {ticker_total} ({pct}%) {:.1}s   ", start.elapsed().as_secs_f32());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    });
    let mut buf = vec![0u8; 8 * 1024];
    let result: Result<(), String> = loop {
        if let Ok(Err(e)) = ack_rx.try_recv() {
            break Err(e); // busy receiver / wrong passphrase — stop early
        }
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => break Err(format!("read {path}: {e}")),
        };
        if n == 0 {
            break Ok(());
        }
        sent_add(n as u64);
        let payload = match tunnel_key() {
            Some(key) => match encrypt(&buf[..n], key) {
                Ok((sealed, nonce)) => with(base.clone(), json!({"type":"chunk","nonce":b64(&nonce),"b64":b64(&sealed)})),
                Err(e) => break Err(e),
            },
            None => with(base.clone(), json!({"type":"chunk","b64":b64(&buf[..n])})),
        };
        if let Err(e) = tx.send(Message::text(payload.to_string())).await {
            break Err(e.to_string());
        }
    };
    ticker.abort();
    if let Err(e) = result {
        reader.abort();
        print!("\r                                          \r");
        return Err(e);
    }
    tx.send(Message::text(with(base.clone(), json!({"type":"end"})).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    print!("\r  ✓ done.                    \n");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // the receiver decrypts and writes as frames arrive; give a slow disk
    // or link plenty of time to finish before calling it a failure
    print!("  waiting for the receiver to confirm…");
    let _ = std::io::stdout().flush();
    let outcome = match tokio::time::timeout(std::time::Duration::from_secs(600), ack_rx.recv()).await {
        Ok(Some(r)) => r,
        Ok(None) => Err("relay closed the connection".into()),
        Err(_) => Err("the receiver did not confirm receipt within 10 minutes".into()),
    };
    reader.abort();
    print!("\r                                          \r");
    outcome?;
    Ok(total as u64)
}

/// Persistent receive loop: connect, register, handle incoming frames.
/// Reconnects automatically until the process exits.
pub async fn receive_loop(relay: String, self_id: String) {
    let group = current_group();
    if group.is_empty() || cfg_snapshot().tunnel.member_token.is_empty() {
        set_status("not in a group — create or join one (Preferences / `whisperdrop group`)");
        println!("[tunnel] no group membership yet — tunnel idle");
        return;
    }
    loop {
        println!("[tunnel] connecting {relay} as {self_id}…");
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
                        let role = v["role"].as_str().unwrap_or("member");
                        update_members(&v["members"]);
                        set_status(format!("online ({role})"));
                        println!("[tunnel] registered as {role} of group {group}");
                    }
                    Err(e) => {
                        println!("[tunnel] registration refused: {e}");
                        set_status(format!("not a member: {e}"));
                        return; // wait for a config change instead of hammering the relay
                    }
                }
                // The relay returns membership only when asked. Poll without
                // reconnecting so peers on another network appear shortly
                // after they start WhisperDrop. Pings + a stale timeout keep
                // zombie links from silently eating transfers.
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
                                handle_frame(&mut tx, &text, &self_id).await
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

async fn handle_frame(
    tx: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    text: &str,
    self_id: &str,
) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    match v["type"].as_str().unwrap_or("") {
        "join_request" => on_join_request(&v),
        "members" => update_members(&v["members"]),
        "registered" | "ok" | "pending" | "join_decided" => {}
        "error" => {
            let e = v["err"].as_str().unwrap_or("relay error");
            println!("[tunnel] relay: {e}");
            set_status(format!("relay: {e}"));
        }
        "devices" => {
            if let Some(arr) = v["ids"].as_array() {
                let ids: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .filter(|id| id != self_id)
                    .collect();
                println!("[tunnel] devices online: {:?}", ids);
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
            let encrypted = v["encrypted"].as_bool().unwrap_or(false);
            if encrypted && tunnel_key().is_none() {
                let reply_to = v["reply_to"].as_str().unwrap_or("");
                if !reply_to.is_empty() {
                    let _ = tx.send(Message::text(json!({"type":"error","to":reply_to,"from":"tunnel-send","err":"encrypted transfer requires the matching pairing passphrase"}).to_string())).await;
                }
                return;
            }
            println!("→ tunnel incoming: {name} ({size} bytes)");
            *IN_NAME.lock().unwrap() = name.clone();
            *IN_REPLY_TO.lock().unwrap() = v["reply_to"].as_str().unwrap_or("").to_string();
            IN_TOTAL.store(
                v["wire_size"].as_u64().unwrap_or(size).max(1),
                Ordering::Relaxed,
            );
            IN_SENT.store(0, Ordering::Relaxed);
            IN_ENCRYPTED.store(encrypted, Ordering::Relaxed);
            IN_FAILED.store(false, Ordering::Relaxed);
            part_close().await;
            // open the .part file the chunks append to
            let dir = crate::receive_dir();
            if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                eprintln!("[tunnel] cannot create {}: {e}", dir.display());
            }
            let safe = crate::sanitize_filename(&name).unwrap_or_else(|| "file".to_string());
            let part = dir.join(format!(".{safe}.{}.part", std::process::id()));
            match tokio::fs::File::create(&part).await {
                Ok(_) => {
                    *IN_NAME.lock().unwrap() = safe.clone();
                    *IN_FILE.lock().unwrap() = Some(part);
                    crate::overlay::begin(&safe, IN_TOTAL.load(Ordering::Relaxed));
                }
                Err(e) => {
                    eprintln!("[tunnel] cannot create {}: {e}", part.display());
                    *IN_FILE.lock().unwrap() = None;
                IN_FROM.lock().unwrap().clear();
                }
            }
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
                                    .send(Message::text(json!({"type":"error","to":reply_to,"from":"tunnel-send","err":err}).to_string()))
                                    .await;
                            }
                            crate::overlay::finish();
                            return;
                        }
                    },
                    None => {
                        IN_FAILED.store(true, Ordering::Relaxed);
                        eprintln!(
                            "[tunnel] encrypted chunk without a pairing passphrase — aborting"
                        );
                        part_close().await;
                        let part = IN_FILE.lock().unwrap().take();
                        if let Some(p) = part {
                            let _ = tokio::fs::remove_file(&p).await;
                        }
                        crate::overlay::finish();
                        return;
                    }
                }
            }
            IN_SENT.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            part_write(&bytes).await;
            crate::overlay::progress(
                IN_SENT.load(Ordering::Relaxed),
                IN_TOTAL.load(Ordering::Relaxed),
            );
        }
        "end" => {
            part_close().await;
            let newname = IN_NAME.lock().unwrap().clone();
            let part_opt = IN_FILE.lock().unwrap().clone();
            if let Some(part) = part_opt {
                if IN_FAILED.swap(false, Ordering::Relaxed) {
                    let _ = tokio::fs::remove_file(&part).await;
                    let reply_to = IN_REPLY_TO.lock().unwrap().clone();
                    if !reply_to.is_empty() {
                        let _ = tx.send(Message::text(json!({"type":"error","to":reply_to,"from":"tunnel-send","err":"decryption failed — passphrase does not match"}).to_string())).await;
                    }
                    *IN_FILE.lock().unwrap() = None;
                IN_FROM.lock().unwrap().clear();
                    crate::overlay::finish();
                    return;
                }
                if let Some(dir) = part.parent() {
                    let mut finalp = dir.join(&newname);
                    let mut i = 1;
                    while finalp.exists() {
                        finalp = dir.join(format!("{newname} ({i})"));
                        i += 1;
                    }
                    tokio::fs::rename(&part, &finalp).await.ok();
                    println!("✓ tunnel received -> {}", finalp.display());
                    let reply_to = IN_REPLY_TO.lock().unwrap().clone();
                    if !reply_to.is_empty() {
                        let _ = tx
                            .send(Message::text(
                                json!({
                                    "type":"complete", "to":reply_to, "from":"tunnel-send",
                                    "name":newname, "size":IN_SENT.load(Ordering::Relaxed)
                                })
                                .to_string(),
                            ))
                            .await;
                    }
                }
                *IN_FILE.lock().unwrap() = None;
                IN_FROM.lock().unwrap().clear();
            }
            crate::overlay::finish();
        }
        _ => {}
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_wrong_key_rejected() {
        let key = [11u8; 32];
        let plaintext = b"WhisperDrop private payload";
        let (sealed, nonce) = encrypt(plaintext, key).unwrap();
        assert_eq!(decrypt(&sealed, &nonce, key).unwrap(), plaintext);
        assert!(decrypt(&sealed, &nonce, [8u8; 32])
            .unwrap_err()
            .contains("passphrase does not match"));
    }

    #[test]
    fn fresh_nonce_per_encryption() {
        let key = [9u8; 32];
        let (sealed_a, nonce_a) = encrypt(b"same plaintext", key).unwrap();
        let (sealed_b, nonce_b) = encrypt(b"same plaintext", key).unwrap();
        assert_ne!(nonce_a, nonce_b, "nonce reuse would break confidentiality");
        assert_ne!(sealed_a, sealed_b);
    }
}
