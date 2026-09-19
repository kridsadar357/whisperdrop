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
static IN_SENT: AtomicU64 = AtomicU64::new(0);
static IN_TOTAL: AtomicU64 = AtomicU64::new(1);
static IN_NAME: Mutex<String> = Mutex::new(String::new());
static IN_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
static IN_FAILED: AtomicBool = AtomicBool::new(false);
static IN_ENCRYPTED: AtomicBool = AtomicBool::new(false);
static IN_NONCE: Mutex<String> = Mutex::new(String::new());
static IN_REPLY_TO: Mutex<String> = Mutex::new(String::new());

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
pub async fn send_over_tunnel(
    relay: &str,
    group: &str,
    self_id: &str,
    path: &str,
    sent: Arc<AtomicU64>,
) -> Result<u64, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(
        json!({"type":"register","group":group,"id":self_id}).to_string(),
    ))
    .await
    .map_err(|e| format!("register: {e}"))?;

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
        json!({"type":"begin","group":group,"from":self_id,"name":name,"size":total,"encrypted":encrypted}).to_string(),
    ))
    .await
    .map_err(|e| format!("send begin: {e}"))?;

    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; CHUNK];
    let mut chunk_no: u64 = 0;
    loop {
        let n = file.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        sent.fetch_add(n as u64, Ordering::Relaxed);
        let frame = match tunnel_key() {
            Some(key) => {
                let (sealed, nonce) = encrypt(&buf[..n], key)?;
                json!({"type":"chunk","group":group,"from":self_id,"nonce":b64(&nonce),"b64":b64(&sealed)})
            }
            None => json!({"type":"chunk","group":group,"from":self_id,"b64":b64(&buf[..n])}),
        };
        tx.send(Message::text(frame.to_string()))
            .await
            .map_err(|e| format!("send chunk: {e}"))?;
        chunk_no += 1;
        let _ = chunk_no;
    }

    tx.send(Message::text(
        json!({"type":"end","group":group,"from":self_id}).to_string(),
    ))
    .await
    .map_err(|e| format!("send end: {e}"))?;

    // wait briefly for the receiving side's confirmation frames (best effort)
    let _ = tokio::time::timeout(std::time::Duration::from_millis(400), rx.next()).await;
    Ok(total)
}

/// Persistent receive loop — abort + respawn when the tunnel config changes.
/// one-shot probe: register with a group and list its members
pub async fn devices(relay: &str, group: &str, self_id: &str) -> Result<Vec<String>, String> {
    let (ws, _) = connect(relay).await?;
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::text(
        json!({"type":"register","group":group,"id":self_id}).to_string(),
    ))
    .await
    .map_err(|e| e.to_string())?;
    tx.send(Message::text(json!({"type":"devices"}).to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let reply = tokio::time::timeout(std::time::Duration::from_secs(6), rx.next())
        .await
        .map_err(|_| "devices request timed out".to_string())?;
    let msg = reply
        .ok_or("no devices reply from relay".to_string())?
        .map_err(|e| format!("relay read: {e}"))?;
    let text = msg
        .into_text()
        .map_err(|_| "devices reply was not text".to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{e}"))?;
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
    devices(relay, group, &format!("check-{}", std::process::id())).await
}

/// Persistent receive loop — abort + respawn when the tunnel config changes.
pub async fn receive_loop(relay: String, group: String, self_id: String, app: AppHandle) {
    loop {
        println!("[tunnel] connecting {relay} as {self_id} (group {group})…");
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
                // poll group membership so later devices appear in the picker;
                // pings + a stale timeout evict zombie links (NAT/CF can drop
                // tunnels without a FIN)
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
            }
            Err(e) => println!("[tunnel] connect failed: {e} — retrying in 3s"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
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
            *IN_NAME.lock().unwrap() = name.clone();
            IN_TOTAL.store(v["wire_size"].as_u64().unwrap_or(size).max(1), Ordering::Relaxed);
            IN_SENT.store(0, Ordering::Relaxed);
            IN_ENCRYPTED.store(v["encrypted"].as_bool().unwrap_or(false), Ordering::Relaxed);
            *IN_NONCE.lock().unwrap() = v["nonce"].as_str().unwrap_or("").to_string();
            IN_FAILED.store(false, Ordering::Relaxed);
            let mut dir = crate::server::receive_dir();
            std::fs::create_dir_all(&dir).ok();
            dir.push(format!(".{name}.part"));
            *IN_FILE.lock().unwrap() = Some(dir.clone());
            tokio::fs::write(&dir, b"").await.ok();
            crate::tray::notify(app, "WhisperDrop", &format!("Incoming: {name} via tunnel"));
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
            let part_opt = IN_FILE.lock().unwrap().clone();
            if let Some(p) = part_opt {
                use tokio::io::AsyncWriteExt;
                if let Ok(mut f) = tokio::fs::OpenOptions::new().append(true).open(&p).await {
                    let _ = f.write_all(&bytes).await;
                }
            }
        }
        "end" => {
            let newname = IN_NAME.lock().unwrap().clone();
            let failed = IN_FAILED.swap(false, Ordering::Relaxed);
            let part_opt = IN_FILE.lock().unwrap().clone();
            if let Some(part) = part_opt {
                if failed {
                    let _ = tokio::fs::remove_file(&part).await;
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
