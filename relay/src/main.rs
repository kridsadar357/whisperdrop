//! WhisperDrop Relay — deploy behind TLS (wss://riki-api.online/ws -> here).
//!
//! Groups are *owned*: the relay issues the 6-digit group id (never reused,
//! never chosen by a client) and a secret member token per device. Joining
//! a group is a request the group's head approves from their own app; only
//! connections that present a valid token are registered, so strangers who
//! guess a number see nothing — no presence, no metadata, no frames.
//! File contents are still end-to-end encrypted by the clients' passphrase;
//! the relay only routes opaque frames between approved members.
//!
//! Frames (JSON text over the WebSocket), any may be the first message:
//!   create_group {device_id, device_name}        -> group_created {group, token}
//!   join_request {group, device_id, device_name} -> join_pending {request_id}
//!                                                    then join_result {request_id, approved, token?}
//!   join_status  {group, request_id}             -> join_result | join_pending
//!   ping                                         -> pong
//!   register     {group, id, token, listen?}     -> registered {role, members:[..]} | error
//!                (listen=false: send-only session, receives only frames addressed "to" it)
//! After register (member):
//!   devices                                      -> devices {ids:[online ids]}
//!   members                                      -> members {members:[{device_id,name,role,online}]}
//!   leave                                        -> ok
//!   <anything else>                              -> forwarded to the other online members
//! After register (head only):
//!   pending                                      -> pending {requests:[{request_id,device_id,device_name,ts}]}
//!   approve {request_id, approved}               -> ok   (requester gets join_result)
//!   kick {device_id}                             -> ok   (their tokens are revoked, links closed)
//! The head also receives join_request {request_id, group, device_id, device_name}
//! pushes — on arrival and for every request still pending when they connect.
//!
//! Run: PORT=8765 GROUPS_FILE=/var/lib/whisperdrop-relay/groups.json ./whisperdrop-relay

use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

// ------------------------------------------------------------ persistence
#[derive(Serialize, Deserialize, Clone, Debug)]
struct MemberRec {
    device_id: String,
    name: String,
    role: String, // "head" | "member"
    added: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct JoinResult {
    approved: bool,
    token: Option<String>,
    decided: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Pending {
    device_id: String,
    name: String,
    ts: u64,
    result: Option<JoinResult>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Group {
    created: u64,
    /// token -> member
    members: HashMap<String, MemberRec>,
    /// request id -> request
    pending: HashMap<String, Pending>,
}

#[derive(Serialize, Deserialize, Default)]
struct Registry {
    groups: HashMap<String, Group>,
}

impl Registry {
    fn load(path: &str) -> Registry {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self, path: &str) {
        if let Some(dir) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = format!("{path}.tmp");
        if let Ok(text) = serde_json::to_string_pretty(self) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
    fn fresh_group_id(&self) -> String {
        let mut rng = rand::thread_rng();
        loop {
            let n = 100_000 + (rng.next_u32() % 900_000);
            let id = n.to_string();
            if !self.groups.contains_key(&id) {
                return id;
            }
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn request_id() -> String {
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// -------------------------------------------------------------- live state
/// Per-connection outbound queue. Bounded so a slow receiver applies
/// backpressure to the sender instead of filling the relay's memory
/// with a whole file; ~256 chunk frames ≈ 3 MB in flight per member.
type OutTx = tokio::sync::mpsc::Sender<Message>;
const QUEUE: usize = 256;

#[derive(Clone)]
struct Conn {
    device_id: String,
    role: String,
    /// receives transfer traffic (an app's receive loop). Send-only
    /// sessions (`register` with `"listen": false`) never do — they only
    /// read their socket after uploading, so pushing to them would stall.
    listen: bool,
    tx: OutTx,
}

/// waiting join requesters: request id -> their outbound channel
type Waiters = HashMap<String, OutTx>;

struct State {
    registry: Registry,
    path: String,
    /// group -> connection key -> connection
    online: HashMap<String, HashMap<u64, Conn>>,
    waiters: Waiters,
}

type Shared = Arc<Mutex<State>>;
static CONN_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl State {
    fn persist(&self) {
        self.registry.save(&self.path);
    }
    fn heads(&self, group: &str) -> Vec<Conn> {
        self.online
            .get(group)
            .map(|m| m.values().filter(|c| c.role == "head").cloned().collect())
            .unwrap_or_default()
    }
    fn others(&self, group: &str, exclude: u64) -> Vec<Conn> {
        self.online
            .get(group)
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| **k != exclude)
                    .map(|(_, c)| c.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Where a transfer frame goes: addressed frames (`to`) reach every
    /// session of that device; broadcast frames reach the *listening*
    /// sessions of every other device (never the sender's own sessions).
    fn targets(&self, group: &str, sender: &str, sender_key: u64, to: Option<&str>) -> Vec<Conn> {
        self.online
            .get(group)
            .map(|m| {
                m.iter()
                    .filter(|(k, c)| **k != sender_key && match to {
                        Some(dev) => c.device_id == dev,
                        None => c.listen && c.device_id != sender,
                    })
                    .map(|(_, c)| c.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn online_ids(&self, group: &str, exclude: u64) -> Vec<String> {
        let mut ids: Vec<String> = self.others(group, exclude).into_iter().map(|c| c.device_id).collect();
        ids.sort();
        ids.dedup();
        ids
    }
    fn members_json(&self, group: &str) -> serde_json::Value {
        let online: std::collections::HashSet<String> = self
            .online
            .get(group)
            .map(|m| m.values().map(|c| c.device_id.clone()).collect())
            .unwrap_or_default();
        let mut list: Vec<serde_json::Value> = self
            .registry
            .groups
            .get(group)
            .map(|g| {
                g.members
                    .values()
                    .map(|m| {
                        json!({
                            "device_id": m.device_id, "name": m.name, "role": m.role,
                            "added": m.added, "online": online.contains(&m.device_id)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!(list)
    }
}

fn text(v: serde_json::Value) -> Message {
    Message::text(v.to_string())
}

fn err(msg: &str) -> Message {
    text(json!({"type": "error", "err": msg}))
}

fn pending_frame(group: &str, id: &str, p: &Pending) -> serde_json::Value {
    json!({
        "type": "join_request", "request_id": id, "group": group,
        "device_id": p.device_id, "device_name": p.name, "ts": p.ts
    })
}

async fn handle_client(shared: Shared, raw: TcpStream) {
    let Ok(ws) = tokio_tungstenite::accept_async(raw).await else { return };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(QUEUE);

    // ---- first frame decides what this connection is ----
    let Some(Ok(Message::Text(first))) = stream.next().await else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&first) else { return };
    let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();

    let listen = v["listen"].as_bool().unwrap_or(true);
    let (group, device_id, role, key) = match v["type"].as_str() {
        Some("create_group") => {
            let (device_id, name) = (s("device_id"), s("device_name"));
            if device_id.is_empty() {
                let _ = sink.send(err("device_id required")).await;
                return;
            }
            let (group, tok) = {
                let mut st = shared.lock().await;
                let group = st.registry.fresh_group_id();
                let tok = token();
                let mut g = Group { created: now(), ..Default::default() };
                g.members.insert(tok.clone(), MemberRec { device_id: device_id.clone(), name, role: "head".into(), added: now() });
                st.registry.groups.insert(group.clone(), g);
                st.persist();
                (group, tok)
            };
            println!("[group] {group} created by {device_id}");
            let _ = sink.send(text(json!({"type": "group_created", "group": group, "token": tok}))).await;
            return;
        }
        Some("join_request") => {
            let (group, device_id, name) = (s("group"), s("device_id"), s("device_name"));
            let rid = request_id();
            let heads = {
                let mut st = shared.lock().await;
                let Some(g) = st.registry.groups.get_mut(&group) else {
                    drop(st);
                    let _ = sink.send(err("no such group")).await;
                    return;
                };
                g.pending.insert(rid.clone(), Pending { device_id: device_id.clone(), name: name.clone(), ts: now(), result: None });
                st.persist();
                st.waiters.insert(rid.clone(), tx.clone());
                st.heads(&group)
            };
            println!("[group] {group}: join request {rid} from {device_id} ({name}) — {} head(s) online", heads.len());
            let _ = sink.send(text(json!({"type": "join_pending", "request_id": rid, "group": group}))).await;
            let frame = pending_frame(&group, &rid, &Pending { device_id, name, ts: now(), result: None });
            for h in heads {
                let _ = h.tx.send(text(frame.clone())).await;
            }
            // keep the socket open until the head decides or the client leaves
            loop {
                tokio::select! {
                    m = rx.recv() => match m {
                        Some(m) => { if sink.send(m).await.is_err() { break; } }
                        None => break,
                    },
                    r = stream.next() => match r {
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    },
                }
            }
            shared.lock().await.waiters.remove(&rid);
            return;
        }
        Some("join_status") => {
            let (group, rid) = (s("group"), s("request_id"));
            let reply = {
                let st = shared.lock().await;
                match st.registry.groups.get(&group).and_then(|g| g.pending.get(&rid)) {
                    Some(Pending { result: Some(r), .. }) => {
                        json!({"type": "join_result", "request_id": rid, "approved": r.approved, "token": r.token})
                    }
                    Some(_) => json!({"type": "join_pending", "request_id": rid, "group": group}),
                    None => json!({"type": "error", "err": "unknown request"}),
                }
            };
            let _ = sink.send(text(reply)).await;
            return;
        }
        Some("ping") => {
            // reachability probe — no membership needed, reveals nothing
            let _ = sink.send(text(json!({"type": "pong", "groups": shared.lock().await.registry.groups.len()}))).await;
            return;
        }
        Some("register") => {
            let (group, id, tok) = (s("group"), s("id"), s("token"));
            let role = {
                let st = shared.lock().await;
                st.registry
                    .groups
                    .get(&group)
                    .and_then(|g| g.members.get(&tok))
                    .filter(|m| m.device_id == id)
                    .map(|m| m.role.clone())
            };
            let Some(role) = role else {
                println!("[-] {id} rejected for group {group} (no valid token)");
                let _ = sink.send(err("not a member of this group — join it from Preferences")).await;
                return;
            };
            let key = CONN_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (group, id, role, key)
        }
        _ => {
            let _ = sink.send(err("expected create_group / join_request / join_status / register")).await;
            return;
        }
    };

    // ---- registered member session ----
    let (members, pending): (serde_json::Value, Vec<serde_json::Value>) = {
        let mut st = shared.lock().await;
        st.online.entry(group.clone()).or_default().insert(key, Conn { device_id: device_id.clone(), role: role.clone(), listen, tx: tx.clone() });
        let members = st.members_json(&group);
        let pending = if role == "head" {
            st.registry
                .groups
                .get(&group)
                .map(|g| g.pending.iter().filter(|(_, p)| p.result.is_none()).map(|(id, p)| pending_frame(&group, id, p)).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        (members, pending)
    };
    println!("[+] {device_id} ({role}{}) online in group {group}", if listen { "" } else { ", send-only" });
    if sink.send(text(json!({"type": "registered", "role": role, "members": members}))).await.is_err() {
        return;
    }
    for p in pending {
        let _ = sink.send(text(p)).await;
    }

    // Writer: drains this connection's queue into the socket on its own
    // task, so a reader blocked on a full peer queue never stops its own
    // outbound traffic (no two-way deadlock between simultaneous senders).
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = std::time::Instant::now();
    let reply_tx = tx.clone();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_seen.elapsed() > std::time::Duration::from_secs(45) {
                    println!("[!] {device_id} stale — dropping");
                    break;
                }
                if reply_tx.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
            read = stream.next() => match read {
                Some(Ok(Message::Text(t))) => {
                    last_seen = std::time::Instant::now();
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();
                    let reply: Option<serde_json::Value> = match v["type"].as_str() {
                        Some("devices") => {
                            let st = shared.lock().await;
                            Some(json!({"type": "devices", "ids": st.online_ids(&group, key)}))
                        }
                        Some("members") => {
                            let st = shared.lock().await;
                            Some(json!({"type": "members", "group": group, "members": st.members_json(&group)}))
                        }
                        Some("leave") => {
                            let mut st = shared.lock().await;
                            if let Some(g) = st.registry.groups.get_mut(&group) {
                                g.members.retain(|_, m| m.device_id != device_id);
                            }
                            st.persist();
                            let _ = reply_tx.send(text(json!({"type": "ok"}))).await;
                            break;
                        }
                        Some("pending") if role == "head" => {
                            let st = shared.lock().await;
                            let list: Vec<serde_json::Value> = st.registry.groups.get(&group)
                                .map(|g| g.pending.iter().filter(|(_, p)| p.result.is_none()).map(|(id, p)| pending_frame(&group, id, p)).collect())
                                .unwrap_or_default();
                            Some(json!({"type": "pending", "requests": list}))
                        }
                        Some("approve") if role == "head" => {
                            let rid = s("request_id");
                            let approved = v["approved"].as_bool().unwrap_or(false);
                            let mut st = shared.lock().await;
                            let outcome = st.registry.groups.get_mut(&group).and_then(|g| {
                                let p = g.pending.get_mut(&rid)?;
                                if p.result.is_some() { return None; }
                                let tok = approved.then(token);
                                if let Some(t) = &tok {
                                    g.members.insert(t.clone(), MemberRec { device_id: p.device_id.clone(), name: p.name.clone(), role: "member".into(), added: now() });
                                }
                                p.result = Some(JoinResult { approved, token: tok.clone(), decided: now() });
                                Some((p.device_id.clone(), tok))
                            });
                            match outcome {
                                Some((who, tok)) => {
                                    st.persist();
                                    println!("[group] {group}: {who} {}", if approved { "approved" } else { "denied" });
                                    let waiter = st.waiters.get(&rid).cloned();
                                    let heads = st.heads(&group);
                                    drop(st);
                                    if let Some(w) = waiter {
                                        let _ = w.send(text(json!({"type": "join_result", "request_id": rid, "approved": approved, "token": tok}))).await;
                                    }
                                    // other head devices should drop the card too
                                    for h in heads {
                                        let _ = h.tx.send(text(json!({"type": "join_decided", "request_id": rid, "approved": approved}))).await;
                                    }
                                    Some(json!({"type": "ok", "request_id": rid}))
                                }
                                None => Some(json!({"type": "error", "err": "request already decided or unknown"})),
                            }
                        }
                        Some("kick") if role == "head" => {
                            let who = s("device_id");
                            let mut st = shared.lock().await;
                            if let Some(g) = st.registry.groups.get_mut(&group) {
                                g.members.retain(|_, m| m.device_id != who || m.role == "head");
                            }
                            st.persist();
                            let victims: Vec<Conn> = st.others(&group, key).into_iter().filter(|c| c.device_id == who).collect();
                            drop(st);
                            for c in victims {
                                let _ = c.tx.send(err("removed from the group by its head")).await;
                                let _ = c.tx.send(Message::Close(None)).await;
                            }
                            Some(json!({"type": "ok"}))
                        }
                        Some("pending") | Some("approve") | Some("kick") => Some(json!({"type": "error", "err": "only the group head can do that"})),
                        _ => {
                            // transfer frame: deliver to every other online *device*
                            // (never back to the sender's own receive session);
                            // awaiting a full queue is the backpressure
                            let to = v["to"].as_str().map(str::to_string);
                            let targets = shared.lock().await.targets(&group, &device_id, key, to.as_deref());
                            for c in targets {
                                let _ = c.tx.send(Message::text(t.clone())).await;
                            }
                            None
                        }
                    };
                    if let Some(r) = reply {
                        if reply_tx.send(text(r)).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => { last_seen = std::time::Instant::now(); }
            },
        }
    }

    {
        let mut st = shared.lock().await;
        if let Some(m) = st.online.get_mut(&group) {
            m.remove(&key);
            if m.is_empty() {
                st.online.remove(&group);
            }
        }
    }
    drop(reply_tx);
    drop(tx);
    writer.abort();
    println!("[-] {device_id} offline (group {group})");
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let path = std::env::var("GROUPS_FILE").unwrap_or_else(|_| "groups.json".into());
    let registry = Registry::load(&path);
    println!("WhisperDrop relay listening on 0.0.0.0:{port} — {} group(s) in {path}", registry.groups.len());
    let shared: Shared = Arc::new(Mutex::new(State { registry, path, online: HashMap::new(), waiters: HashMap::new() }));
    let listener = TcpListener::bind(("0.0.0.0", port)).await.expect("bind failed");
    loop {
        if let Ok((raw, _)) = listener.accept().await {
            tokio::spawn(handle_client(shared.clone(), raw));
        }
    }
}
