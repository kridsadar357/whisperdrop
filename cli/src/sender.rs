//! Outbound streaming sender — push a file to another WhisperDrop peer.
//! Same wire format as the Mac app: POST /upload?filename=..., body is the
//! raw file stream (constant memory, any file size).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_util::io::ReaderStream;

use futures_util::StreamExt;

pub async fn send_file(target_ip: &str, target_port: u16, file_path: &str) -> Result<u64, String> {
    send_file_with_progress(target_ip, target_port, file_path, Arc::new(AtomicU64::new(0)), None).await
}

/// Like `send_file`, but bytes sent are published to `sent` (for the drop
/// zone's progress bars) and the file size to `total` once known.
pub async fn send_file_with_progress(
    target_ip: &str,
    target_port: u16,
    file_path: &str,
    sent: Arc<AtomicU64>,
    total_out: Option<Arc<AtomicU64>>,
) -> Result<u64, String> {
    let filename = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid file path: {file_path}"))?
        .to_string();

    let file = tokio::fs::File::open(file_path)
        .await
        .map_err(|e| format!("open {file_path}: {e}"))?;
    let total = file.metadata().await.map_err(|e| e.to_string())?.len();
    if let Some(t) = &total_out {
        t.store(total, Ordering::Relaxed);
    }

    sent.store(0, Ordering::Relaxed);
    let counter = sent.clone();
    let stream = ReaderStream::new(file).map(move |chunk| {
        if let Ok(ref bytes) = chunk {
            counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        chunk
    });

    // live console progress (carriage-return line)
    let ticker_sent = sent.clone();
    let ticker = tokio::spawn(async move {
        let mut last: u64 = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let cur = ticker_sent.load(Ordering::Relaxed);
            if cur != last {
                let pct = if total > 0 {
                    (cur as f64 / total as f64 * 100.0).round() as u64
                } else {
                    100
                };
                print!("\r  ↑ {} / {} ({}%)   ", cur, total, pct);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                last = cur;
            }
        }
    });

    let url = format!(
        "http://{target_ip}:{target_port}/upload?filename={}",
        urlencode(&filename)
    );
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(6))
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| format!("client init: {e}"))?;
    let resp = client
        .post(url)
        .header("content-length", total)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("timed out") {
                "connection timed out — peer unreachable (firewall?)".to_string()
            } else {
                msg
            }
        })?;
    ticker.abort();

    if !resp.status().is_success() {
        return Err(format!("peer rejected upload: {}", resp.status()));
    }
    let _ = total;
    Ok(total)
}

/// Resolve a user-typed peer: either a number (index into mdns::list())
/// or an IP string (probe the port range).
pub async fn resolve(input: &str) -> Result<crate::mdns::Peer, String> {
    let input = input.trim();
    if input.chars().all(|c| c.is_ascii_digit()) && !input.is_empty() {
        let idx: usize = input
            .parse()
            .map_err(|_| "invalid peer number".to_string())?;
        let peers = crate::mdns::list();
        peers
            .get(idx.saturating_sub(1))
            .cloned()
            .ok_or_else(|| format!("no peer number {input} (try 'list' first)"))
    } else if input.chars().all(|c| c.is_ascii_digit() || c == '.') && input.contains('.') {
        // bare IP: probe the standard port range
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| e.to_string())?;
        for port in [51730u16, 51731, 51732] {
            let url = format!("http://{input}:{port}/health");
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    return Ok(crate::mdns::Peer {
                        name: input.to_string(),
                        ip: input.to_string(),
                        port,
                    });
                }
            }
        }
        Err(format!("no receiver answered on {input}:51730-51732"))
    } else {
        // peer NAME
        crate::mdns::list()
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(input))
            .ok_or_else(|| format!("no peer named {input} (try 'list')"))
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
