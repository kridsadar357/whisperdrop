use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_bridge-service._tcp.local.";

#[derive(Clone, serde::Serialize)]
pub struct Peer {
    pub name: String,
    pub ip: String,
    pub port: u16,
}

/// Peers seen recently; entries expire after EXPIRY without a refresh.
struct PeerCache {
    // Held (never read) so the mDNS daemon lives as long as the cache.
    #[allow(dead_code)]
    daemon: Option<ServiceDaemon>,
    peers: HashMap<String, (Peer, Instant)>,
}

static CACHE: Mutex<Option<PeerCache>> = Mutex::new(None);
const EXPIRY: Duration = Duration::from_secs(75);

/// Register our own service and start browsing for peers. `host_name`
/// identifies this device on the LAN; must be unique per machine.
pub fn start(host_name: &str, port: u16) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[mdns] daemon failed: {e}");
            return;
        }
    };

    let info = ServiceInfo::new(
        SERVICE_TYPE,
        host_name,
        &format!("{host_name}.local."),
        mdns_local_ip().unwrap_or_else(|| "0.0.0.0".into()),
        port,
        None,
    )
    .expect("valid service info");
    if let Err(e) = daemon.register(info) {
        eprintln!("[mdns] register failed: {e}");
    }

    let browser = match daemon.browse(SERVICE_TYPE) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[mdns] browse failed: {e}");
            return;
        }
    };

    *CACHE.lock().unwrap() = Some(PeerCache {
        daemon: Some(daemon),
        peers: HashMap::new(),
    });

    std::thread::spawn(move || {
        while let Ok(event) = browser.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    if let Some(ip) = info.get_addresses().iter().next() {
                        let name = info
                            .get_fullname()
                            .trim_end_matches('.')
                            .split('.')
                            .next()
                            .unwrap_or("peer")
                            .to_string();
                        let peer = Peer {
                            name: name.clone(),
                            ip: ip.to_string(),
                            port: info.get_port(),
                        };
                        eprintln!("[mdns] peer up: {} at {}:{}", peer.name, peer.ip, peer.port);
                        if let Ok(Some(cache)) = CACHE.lock().as_deref_mut() {
                            cache
                                .peers
                                .insert(format!("{name}/{ip}"), (peer, Instant::now()));
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let key = fullname
                        .trim_end_matches('.')
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    eprintln!("[mdns] peer down: {key}");
                    if let Ok(Some(cache)) = CACHE.lock().as_deref_mut() {
                        cache
                            .peers
                            .retain(|k, _| !k.starts_with(&format!("{key}/")));
                    }
                }
                _ => {}
            }
        }
    });
}

/// Current online peers (excluding ourselves), pruned of stale entries.
pub fn online_peers() -> Vec<Peer> {
    let mut out = Vec::new();
    if let Ok(Some(cache)) = CACHE.lock().as_deref_mut() {
        cache.peers.retain(|_, (_, seen)| seen.elapsed() < EXPIRY);
        for (peer, _) in cache.peers.values() {
            if is_own_ip(&peer.ip) {
                continue;
            }
            out.push(peer.clone());
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn mdns_local_ip() -> Option<String> {
    let local: std::net::IpAddr = "192.168.0.0".parse().ok()?;
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect((local, 1)).ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn is_own_ip(ip: &str) -> bool {
    mdns_local_ip().as_deref() == Some(ip)
}
