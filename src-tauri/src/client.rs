use futures_util::StreamExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio_util::io::ReaderStream;

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub transfer_id: String,
    pub filename: String,
    pub sent: u64,
    pub total: u64,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferDone {
    pub transfer_id: String,
    pub filename: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Stream a file to target_ip:target_port /upload, emitting
/// `transfer-progress` (~20/sec) and `transfer-done` Tauri events.
pub async fn send_file_with_progress(
    app: &AppHandle,
    transfer_id: &str,
    target_ip: &str,
    target_port: u16,
    file_path: &str,
) -> Result<u64, String> {
    let result = send_file_inner(target_ip, target_port, file_path, Some((app, transfer_id))).await;
    let done = match &result {
        Ok((filename, _)) => TransferDone {
            transfer_id: transfer_id.to_string(),
            filename: filename.clone(),
            ok: true,
            error: None,
        },
        Err(e) => TransferDone {
            transfer_id: transfer_id.to_string(),
            filename: std::path::Path::new(file_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ok: false,
            error: Some(e.clone()),
        },
    };
    let _ = app.emit("transfer-done", done);
    result.map(|(_, n)| n)
}

/// Plain streaming send with no Tauri dependency (used by tests).
pub async fn send_file_stream(
    target_ip: &str,
    target_port: u16,
    file_path: &str,
) -> Result<u64, String> {
    send_file_inner(target_ip, target_port, file_path, None)
        .await
        .map(|(_, n)| n)
}

type ProgressCtx<'a> = (&'a AppHandle, &'a str);

/// Core: stream a file from disk to the peer without buffering it in
/// memory — ReaderStream pulls chunks which reqwest forwards as a
/// chunked transfer-encoding body. Returns (filename, bytes_sent).
async fn send_file_inner(
    target_ip: &str,
    target_port: u16,
    file_path: &str,
    progress: Option<ProgressCtx<'_>>,
) -> Result<(String, u64), String> {
    let filename = std::path::Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid file path: {file_path}"))?
        .to_string();

    let file = tokio::fs::File::open(file_path)
        .await
        .map_err(|e| format!("open {file_path}: {e}"))?;
    let total = file.metadata().await.map_err(|e| e.to_string())?.len();

    // Count bytes as they leave the stream; emit throttled progress events
    // directly from the stream closure (Emitter::emit is synchronous).
    let sent = Arc::new(AtomicU64::new(0));
    let mut last_emit = std::time::Instant::now();
    let ctx = progress.map(|(app, id)| (app.clone(), id.to_string()));
    let counter = sent.clone();
    let stream_filename = filename.clone();

    let stream = ReaderStream::new(file).map(move |chunk| {
        if let (Ok(ref bytes), Some((ref app, ref id))) = (&chunk, &ctx) {
            let n = counter.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
            if last_emit.elapsed() >= std::time::Duration::from_millis(50) {
                last_emit = std::time::Instant::now();
                let _ = app.emit(
                    "transfer-progress",
                    TransferProgress {
                        transfer_id: id.clone(),
                        filename: stream_filename.clone(),
                        sent: n,
                        total,
                    },
                );
            }
        }
        chunk
    });

    let url = format!(
        "http://{target_ip}:{target_port}/upload?filename={}",
        urlencoding_minimal(&filename)
    );
    let client = reqwest::Client::builder()
        // Firewall-silent-drops otherwise leave the widget at 0% forever.
        .connect_timeout(std::time::Duration::from_secs(6))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| format!("client init: {e}"))?;
    let resp = client
        .post(url)
        .header("content-length", total)
        .body(reqwest::Body::wrap_stream(stream))
        // Overall cap for the transfer itself.
        .timeout(std::time::Duration::from_secs(3600))
        .send()
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("timed out") {
                "connection timed out — target unreachable (Windows Firewall must allow TCP 51730)"
                    .to_string()
            } else if msg.contains("refused") {
                "connection refused — is WhisperDrop running on the target?".to_string()
            } else {
                msg
            }
        })?;

    if !resp.status().is_success() {
        return Err(format!("peer rejected upload: {}", resp.status()));
    }
    eprintln!("[client] sent {file_path} ({total} bytes) to {target_ip}:{target_port}");
    Ok((filename, total))
}

/// Percent-encode the minimum set required for a query-string value.
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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
