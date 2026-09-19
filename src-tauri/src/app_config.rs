//! Shared persistent config — same file as the Windows receiver:
//! ~/Library/Application Support/WhisperDrop/config.json (macOS)
//! %APPDATA%\WhisperDrop\config.json (Windows)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TunnelCfg {
    pub enabled: bool,
    pub relay: String,
    pub device_id: String,
    /// A pairing passphrase shared only with trusted devices. It is used to
    /// derive the end-to-end tunnel encryption key; never sent to the relay.
    #[serde(default)]
    pub shared_secret: String,
    /// Secret issued by the relay when this device was admitted to the
    /// group (or created it). Never shared between devices.
    #[serde(default)]
    pub member_token: String,
    /// "head" | "member" | "" — as reported by the relay.
    #[serde(default)]
    pub role: String,
    /// A join request still waiting for the head's decision.
    #[serde(default)]
    pub pending_request: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// "left" | "right" — where the edge drop zone lives
    pub position: String,
    pub wizard_done: bool,
    pub tunnel: TunnelCfg,
    /// where the app checks for update manifests (latest.json)
    #[serde(default = "default_updates_url")]
    pub updates_url: String,
    /// where incoming files are saved
    #[serde(default = "default_receive_dir")]
    pub receive_dir: String,
    /// Trusted tunnel IDs entered manually when a relay cannot provide its
    /// membership list. These are device IDs, not IP addresses.
    #[serde(default)]
    pub known_tunnel_devices: Vec<String>,
    /// 6-digit group id — devices sharing this id (and the passphrase)
    /// can send to each other
    #[serde(default)]
    pub group_id: String,
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
                device_id: String::from("dev"),
                shared_secret: String::new(),
                member_token: String::new(),
                role: String::new(),
                pending_request: String::new(),
            },
            updates_url: default_updates_url(),
            receive_dir: default_receive_dir(),
            known_tunnel_devices: Vec::new(),
            group_id: String::new(),
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
