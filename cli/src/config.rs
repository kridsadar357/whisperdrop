//! Persistent config: %APPDATA%\WhisperDrop\config.json (Windows) or
//! ~/Library/Application Support/WhisperDrop/config.json (macOS).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct TunnelCfg {
    pub enabled: bool,
    pub relay: String,
    pub device_id: String,
    /// Shared manually between trusted devices; it never leaves either peer.
    #[serde(default)]
    pub shared_secret: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    /// "left" | "right" — where the edge drop zone + overlay live
    pub position: String,
    pub wizard_done: bool,
    pub tunnel: TunnelCfg,
    /// 6-digit group id — devices sharing this id (and the passphrase)
    /// can send to each other
    #[serde(default)]
    pub group_id: String,
    #[serde(default = "default_receive_dir")]
    pub receive_dir: String,
    /// Trusted IDs provide a manual tunnel fallback when membership lookup is
    /// unavailable at the relay.
    #[serde(default)]
    pub known_tunnel_devices: Vec<String>,
    /// where the app checks for update manifests (latest.json)
    #[serde(default = "default_updates_url")]
    pub updates_url: String,
}

fn default_updates_url() -> String {
    String::from("https://riki-api.online/whisperdrop/updates/latest.json")
}

fn default_receive_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("BridgeReceived")
        .display()
        .to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            position: "right".into(),
            wizard_done: false,
            tunnel: TunnelCfg {
                enabled: false,
                relay: "wss://riki-api.online/ws".into(),
                device_id: default_device_id(),
                shared_secret: String::new(),
            },
            group_id: String::new(),
            receive_dir: default_receive_dir(),
            known_tunnel_devices: Vec::new(),
            updates_url: default_updates_url(),
        }
    }
}

pub fn config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("WhisperDrop");
    p.push("config.json");
    p
}

pub fn load() -> Config {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&text) {
            return cfg;
        }
    }
    Config::default()
}

pub fn save(cfg: &Config) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// Stable short identity: hostname hash + random suffix, e.g. "tkgh-a1b2c3d4"
fn default_device_id() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "dev".into());
    let mut h: u64 = 1469598103934665603; // FNV offset basis
    for b in host.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    let rand_part = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 20;
    h ^= rand_part.wrapping_mul(0x9E3779B97F4A7C15);
    format!(
        "{}-{:06x}",
        host.chars().take(4).collect::<String>().to_lowercase(),
        h & 0xFF_FFFF
    )
}
