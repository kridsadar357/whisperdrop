//! mDNS: announce this machine AND browse for WhisperDrop peers.
//! `list()` returns currently visible peers (self excluded).

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::sync::Mutex;

pub const SERVICE_TYPE: &str = "_bridge-service._tcp.local.";

#[derive(Clone, Debug)]
pub struct Peer {
    pub name: String,
    pub ip: String,
    pub port: u16,
}

static PEERS: Mutex<Option<HashMap<String, Peer>>> = Mutex::new(None);
static SELF_INSTANCE: Mutex<String> = Mutex::new(String::new());
/// Suppress "peer up/down" console lines (CLI one-shot commands).
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn start(host: &str, port: u16) {
    start_inner(host, Some(port));
}

/// Browse for peers without announcing this process (CLI `send`/`devices`).
pub fn browse_only() {
    QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    start_inner("", None);
}

fn start_inner(host: &str, port: Option<u16>) {
    *SELF_INSTANCE.lock().unwrap() = host.to_string();
    let host = host.to_string();
    std::thread::spawn(move || {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[mdns] daemon failed: {e}");
                return;
            }
        };
        let local_ip: String = std::net::UdpSocket::bind(("0.0.0.0", 0))
            .and_then(|s| {
                s.connect(("10.255.255.255", 1))?;
                s.local_addr()
            })
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "0.0.0.0".to_string());
        if let Some(port) = port {
            let info = ServiceInfo::new(
                SERVICE_TYPE,
                &host,
                &format!("{host}.local."),
                &local_ip,
                port,
                None,
            )
            .expect("valid service info");
            if let Err(e) = daemon.register(info) {
                eprintln!("[mdns] register failed: {e}");
            } else {
                println!("[mdns] announced as {host} on {local_ip}:{port}");
            }
        }

        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[mdns] browse failed: {e}");
                return;
            }
        };
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let fullname = info.get_fullname();
                    let instance = fullname
                        .trim_end_matches('.')
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if instance.eq_ignore_ascii_case(&SELF_INSTANCE.lock().unwrap().clone()) {
                        continue; // ourselves
                    }
                    if let Some(ip) = info.get_addresses().iter().next() {
                        let peer = Peer {
                            name: instance.clone(),
                            ip: ip.to_string(),
                            port: info.get_port(),
                        };
                        if !QUIET.load(std::sync::atomic::Ordering::Relaxed) {
                            println!("[mdns] peer up: {} at {}:{}", peer.name, peer.ip, peer.port);
                        }
                        PEERS
                            .lock()
                            .unwrap()
                            .get_or_insert_with(HashMap::new)
                            .insert(format!("{}/{}", instance, peer.ip), peer);
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let instance = fullname
                        .trim_end_matches('.')
                        .split('.')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if let Some(map) = PEERS.lock().unwrap().as_mut() {
                        map.retain(|k, _| !k.starts_with(&format!("{instance}/")));
                    }
                }
                _ => {}
            }
        }
    });
}

/// Discovered peers, sorted by name.
pub fn list() -> Vec<Peer> {
    let guard = PEERS.lock().unwrap();
    let mut out: Vec<Peer> = guard
        .as_ref()
        .map(|map| map.values().cloned().collect())
        .unwrap_or_default();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
