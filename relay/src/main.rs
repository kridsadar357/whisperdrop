//! WhisperDrop Relay — deploy on riki-api.online (behind TLS termination,
//! e.g. nginx/caddy serving wss://riki-api.online/ws -> this server).
//!
//! Group-based routing: devices register with a 6-digit GROUP ID; every
//! transfer frame addressed to that group is delivered to all OTHER
//! members of the group. No per-device addressing, no QR.
//!
//! Run: PORT=8080 ./whisperdrop-relay

use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

#[derive(Clone)]
struct Member {
    id: String,
    tx: tokio::sync::mpsc::UnboundedSender<Message>,
}

/// group id -> (connection key -> member)
type Store = Arc<Mutex<HashMap<String, HashMap<u64, Member>>>>;
static CONN_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

async fn group_members(store: &Store, group: &str, exclude_key: u64) -> Vec<Member> {
    let map = store.lock().await;
    map.get(group)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| **k != exclude_key)
                .map(|(_, member)| member.clone())
                .collect()
        })
        .unwrap_or_default()
}

async fn handle_client(store: Store, raw: TcpStream) {
    let ws = tokio_tungstenite::accept_async(raw).await;
    let Ok(ws) = ws else { return };
    let (mut sink, mut stream) = ws.split();

    // first message must be the registration
    let Some(Ok(Message::Text(first))) = stream.next().await else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&first) else { return };
    if v["type"].as_str() != Some("register") {
        return;
    }
    let Some(group) = v["group"].as_str().map(String::from) else { return };
    let Some(id) = v["id"].as_str().map(String::from) else { return };

    let key = CONN_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    {
        let mut map = store.lock().await;
        map.entry(group.clone()).or_default().insert(key, Member { id: id.clone(), tx });
    }
    println!("[+] {id} joined group {group} ({} in group)", {
        let map = store.lock().await;
        map.get(&group).map(|g| g.len()).unwrap_or(0)
    });

    // single loop: ping stale links, flush queued frames, route group frames.
    // Pings + a stale timeout evict zombie connections (NAT/CF can drop
    // tunnels without a FIN — a stale entry would silently eat transfers).
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = std::time::Instant::now();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_seen.elapsed() > std::time::Duration::from_secs(45) {
                    println!("[!] client {id} stale — dropping");
                    break;
                }
                if sink.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
            msg = rx.recv() => match msg {
                Some(m) => {
                    if sink.send(m).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            read = stream.next() => match read {
                Some(Ok(Message::Text(text))) => {
                    last_seen = std::time::Instant::now();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        match v["type"].as_str() {
                            Some("devices") => {
                                let members =
                                    group_members(&store, &group, key).await;
                                let ids: Vec<String> =
                                    members.iter().map(|m| m.id.clone()).collect();
                                let reply = serde_json::json!({
                                    "type": "devices", "ids": ids
                                });
                                if sink.send(Message::text(reply.to_string())).await.is_err() {
                                    break;
                                }
                            }
                            _ => {
                                // deliver to every other member of the group
                                let members =
                                    group_members(&store, &group, key).await;
                                for m in members {
                                    let _ = m.tx.send(Message::text(text.clone()));
                                }
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {
                    last_seen = std::time::Instant::now();
                }
            },
        }
    }

    // remove only THIS connection from its group
    {
        let mut map = store.lock().await;
        if let Some(members) = map.get_mut(&group) {
            members.remove(&key);
            if members.is_empty() {
                map.remove(&group);
            }
        }
    }
    println!("[-] {id} left group {group}");
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let store: Store = Arc::new(Mutex::new(HashMap::new()));
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind failed");
    println!("WhisperDrop relay listening on 0.0.0.0:{port} (group mode)");
    loop {
        if let Ok((raw, _)) = listener.accept().await {
            let store = store.clone();
            tokio::spawn(handle_client(store, raw));
        }
    }
}
