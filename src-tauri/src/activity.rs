//! Small, dependency-free activity log for the tray "View Activity Log" action.
//! Keep it intentionally best-effort: logging must never prevent a transfer.

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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{now}] {}", message.as_ref());
    }
}
