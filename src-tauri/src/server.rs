use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart},
    http::StatusCode,
    routing::{get, post},
    Router,
};
use futures_util::StreamExt;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

pub const DEFAULT_PORT: u16 = 51730;
pub const RECEIVE_DIR_NAME: &str = "BridgeReceived";

/// Received files land in ~/Downloads/BridgeReceived/
pub fn receive_dir() -> PathBuf {
    let configured = crate::get_cfg().receive_dir;
    if configured.trim().is_empty() {
        let mut d = dirs::download_dir()
            .or_else(dirs::home_dir)
            .expect("no home directory");
        d.push(RECEIVE_DIR_NAME);
        d
    } else {
        PathBuf::from(configured)
    }
}

/// Sanitize a client-provided filename: keep only the final path component
/// and reject empty / dot-names so streams can't escape the receive dir.
fn sanitize_filename(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() || name.starts_with('.') {
        return None;
    }
    Some(name.to_string())
}

/// Deduplicate collisions on disk: "a.txt" -> "a (1).txt".
fn unique_path(dir: &std::path::Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = candidate.extension().and_then(|s| s.to_str());
    for i in 1..u32::MAX {
        let name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let p = dir.join(name);
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}

/// POST /upload?filename=... — body is the raw file byte stream, chunked.
async fn upload(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    stream: Body,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    let filename = sanitize_filename(
        params
            .get("filename")
            .map(String::as_str)
            .unwrap_or("unnamed"),
    )
    .ok_or((StatusCode::BAD_REQUEST, "invalid filename".into()))?;

    let dir = receive_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let dest = unique_path(&dir, &filename);

    let mut file = tokio::fs::File::create(&dest)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut stream = stream.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        written += chunk.len() as u64;
    }
    file.flush()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    eprintln!("[server] received {written} bytes -> {}", dest.display());
    crate::activity::write(format!(
        "received {filename} ({written} bytes) -> {}",
        dest.display()
    ));
    Ok((
        StatusCode::OK,
        dest.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
    ))
}

/// POST /upload-multipart — multipart/form-data variant (each part one file).
async fn upload_multipart(
    mut multipart: Multipart,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    let dir = receive_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut saved: Vec<String> = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let filename = field
            .file_name()
            .and_then(sanitize_filename)
            .unwrap_or_else(|| "unnamed".to_string());
        let dest = unique_path(&dir, &filename);
        let mut file = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut written: u64 = 0;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            written += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        eprintln!("[server] received {written} bytes -> {}", dest.display());
        crate::activity::write(format!(
            "received {filename} ({written} bytes) -> {}",
            dest.display()
        ));
        saved.push(
            dest.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
        );
    }
    Ok((StatusCode::OK, saved.join(", ")))
}

async fn health() -> &'static str {
    "bridge-ok"
}

/// Background receiver server; falls forward from DEFAULT_PORT on collision
/// (a second instance on the same machine gets the next free port).
pub async fn start_receiver_server() -> u16 {
    for port in DEFAULT_PORT..DEFAULT_PORT + 20 {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        let app = Router::new()
            .route("/upload", post(upload))
            .route("/upload-multipart", post(upload_multipart))
            .route("/health", get(health))
            .layer(DefaultBodyLimit::disable());
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                eprintln!("[server] listening on {addr}");
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        eprintln!("[server] fatal: {e}");
                    }
                });
                return port;
            }
            Err(e) => eprintln!("[server] port {port} busy ({e}), trying next"),
        }
    }
    eprintln!("[server] no free port found");
    DEFAULT_PORT
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    // Loopback integration test: stream 50MB of random-ish bytes through the
    // axum server to disk, then verify SHA256 of source and destination match.
    #[tokio::test]
    async fn streams_50mb_loopback_with_matching_sha256() {
        let port = start_receiver_server().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Build a 50MB source file with a repeating pseudo-random pattern.
        // (Named explicitly because tempfile's default ".tmp" prefix would
        // be rejected by the server's filename sanitizer.)
        let src_path =
            std::env::temp_dir().join(format!("loopback-50mb-{}.bin", std::process::id()));
        {
            use tokio::io::AsyncWriteExt;
            let block: Vec<u8> = (0..1024u32)
                .map(|i| i.wrapping_mul(2654435761) as u8)
                .collect();
            let mut f = tokio::fs::File::create(&src_path).await.unwrap();
            for _ in 0..(50 * 1024) {
                f.write_all(&block).await.unwrap();
            }
        }
        let src = &src_path;

        let src_hash = tokio::task::spawn_blocking({
            let p = src.to_path_buf();
            move || sha256_file(&p)
        })
        .await
        .unwrap()
        .unwrap();

        crate::client::send_file_stream("127.0.0.1", port, src.to_str().unwrap())
            .await
            .expect("transfer failed");

        let dest = receive_dir().join(src.file_name().unwrap());
        let dest_hash = tokio::task::spawn_blocking({
            let p = dest.clone();
            move || sha256_file(&p)
        })
        .await
        .unwrap()
        .unwrap();

        let src_len = std::fs::metadata(src).unwrap().len();
        let dest_len = std::fs::metadata(&dest).unwrap().len();
        assert_eq!(src_len, dest_len, "size mismatch");
        assert_eq!(src_hash, dest_hash, "sha256 mismatch");
        std::fs::remove_file(&dest).ok();
    }

    fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
        let mut hasher = Sha256::new();
        let mut f = std::fs::File::open(path)?;
        std::io::copy(&mut f, &mut hasher)?;
        Ok(format!("{:x}", hasher.finalize()))
    }
}
