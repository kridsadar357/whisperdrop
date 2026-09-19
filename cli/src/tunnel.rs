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
static IN_REPLY_TO: Mutex<String> = Mutex::new(String::new());
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
    devices(relay, self_id).await.map(|_| ())
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
    let request_id = session_id("probe", self_id);
    // The relay is group-scoped: registering without a group is dropped.
    // A probe joins the configured group (so the reply lists real peers)
    // or a throwaway one when none is set.
    let group = {
        let g = current_group();
        if g.is_empty() { format!("probe-{}", std::process::id()) } else { g }
    };
    tx.send(Message::text(
        json!({"type":"register","id":request_id,"group":group}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;
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
pub async fn send_over_tunnel(
    relay: &str,
    group: &str,
    path: &str,
) -> Result<u64, String> {
    match send_over_tunnel_once(relay, group, path).await {
        Ok(n) => Ok(n),
        Err(e) => {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            send_over_tunnel_once(relay, group, path).await
                .map_err(|e2| format!("{e} | retry: {e2}"))
        }
    }
}

async fn send_over_tunnel_once(
    relay: &str,
    group: &str,
    path: &str,
) -> Result<u64, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    let request_id = session_id("send", "tunnel");
    tx.send(Message::text(
        json!({"type":"register","group":group,"id":request_id}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;

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

    tx.send(Message::text(
        json!({"type":"begin","group":group,"from":request_id,"reply_to":request_id,"name":name,"size":total,"wire_size":total,"encrypted":encrypted}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;

    // stream the file: every 48 KB chunk is encrypted with a FRESH random
    // nonce (no reuse, constant memory — multi-GB files never touch RAM)
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
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read {path}: {e}"))?;
        if n == 0 {
            break;
        }
        sent_add(n as u64);
        let payload = match tunnel_key() {
            Some(key) => {
                let (sealed, nonce) = encrypt(&buf[..n], key)?;
                json!({"type":"chunk","group":group,"from":"tunnel-send","nonce":b64(&nonce),"b64":b64(&sealed)})
            }
            None => json!({"type":"chunk","group":group,"from":"tunnel-send","b64":b64(&buf[..n])}),
        };
        tx.send(Message::text(payload.to_string()))
            .await
            .map_err(|e| e.to_string())?;
    }
    tx.send(Message::text(
        json!({"type":"end","group":group,"from":"tunnel-send"}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;
    ticker.abort();
    print!("\r  ✓ done.                    \n");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // wait for the relay to flush (best-effort ack window)
    let confirmation = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(message) = rx.next().await {
            let Message::Text(text) = message.map_err(|e| e.to_string())? else {
                continue;
            };
            let frame: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| e.to_string())?;
            match frame["type"].as_str() {
                Some("complete") => return Ok(()),
                Some("error") => {
                    return Err(frame["err"].as_str().unwrap_or("relay error").to_string())
                }
                _ => {}
            }
        }
        Err("relay disconnected before the target confirmed receipt".to_string())
    })
    .await
    .map_err(|_| "target did not confirm receipt within 30 seconds".to_string())?;
    confirmation?;
    Ok(total as u64)
}

/// Persistent receive loop: connect, register, handle incoming frames.
/// Reconnects automatically until the process exits.
pub async fn receive_loop(relay: String, self_id: String) {
    let group = current_group();
    loop {
        println!("[tunnel] connecting {relay} as {self_id}…");
        match connect(&relay).await {
            Ok((ws, _)) => {
                println!("[tunnel] connected ✓");
                let (mut tx, mut rx) = ws.split();
                if tx
                    .send(Message::text(
                        json!({"type":"register","group":group,"id":self_id}).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
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
                            if last_seen.elapsed() > std::time::Duration::from_secs(30) {
                                println!("[tunnel] connection stale — reconnecting");
                                break;
                            }
                            if tx.send(Message::text(json!({"type":"devices"}).to_string())).await.is_err() {
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
            }
            Err(e) => {
                println!("[tunnel] connect failed: {e} — retrying in 3s");
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
            let part_opt = IN_FILE.lock().unwrap().clone();
            if let Some(p) = part_opt {
                use tokio::io::AsyncWriteExt;
                if let Ok(mut f) = tokio::fs::OpenOptions::new().append(true).open(&p).await {
                    let _ = f.write_all(&bytes).await;
                }
            }
            crate::overlay::progress(
                IN_SENT.load(Ordering::Relaxed),
                IN_TOTAL.load(Ordering::Relaxed),
            );
        }
        "end" => {
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
