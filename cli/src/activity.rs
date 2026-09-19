//! Best-effort local activity log. It is deliberately tiny so logging cannot
//! interfere with receiving a file on a busy machine.

use std::io::Write;
use std::path::PathBuf;

pub fn path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("WhisperDrop");
    p.push("activity.log");
    p
}

pub fn write(message: impl AsRef<str>) {
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(out, "[{stamp}] {}", message.as_ref());
    }
}

pub fn recent() -> String {
    std::fs::read_to_string(path()).unwrap_or_else(|_| "No activity recorded yet.".to_string())
}
