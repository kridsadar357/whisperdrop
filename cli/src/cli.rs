//! Command-line interface.
//!
//! `whisperdrop` with no subcommand runs the app (receive files, edge drop
//! zone, tray, tunnel). The subcommands are one-shot: they talk to peers
//! directly from the calling shell and exit, so they work in scripts and
//! alongside a running app.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "whisperdrop",
    version,
    about = "Send and receive files over your LAN or the encrypted tunnel",
    long_about = "WhisperDrop — drag a file to the screen edge, or use the command line.\n\n\
                  With no subcommand the full app runs: it receives files, shows the edge\n\
                  drop zone and tray icon, and keeps the tunnel connected."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    // ---- internal / legacy flags (kept for the installer and wizard) ----
    /// Open the setup wizard on start
    #[arg(long, hide = true)]
    pub wizard: bool,
    /// (Windows, elevated child) create the firewall rule and exit
    #[arg(long, hide = true)]
    pub setup_firewall: bool,
    /// Loop a fake incoming transfer to preview the overlay
    #[arg(long, hide = true)]
    pub demo_overlay: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the app: receive, edge drop zone, tray, tunnel (the default)
    Run,
    /// Send file(s) to a device on the LAN or to your tunnel group
    Send {
        /// File(s) to send
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
        /// Device name, IP[:port], or 6-digit group id (tunnel).
        /// Omitted: the only device on the LAN is used.
        #[arg(short, long, value_name = "TARGET")]
        to: Option<String>,
        /// Send through the relay to the configured group
        #[arg(long)]
        tunnel: bool,
        /// Pairing passphrase for this run only (tunnel; not saved)
        #[arg(long, value_name = "PASSPHRASE")]
        secret: Option<String>,
        /// Relay URL for this run only
        #[arg(long, value_name = "WSS_URL")]
        relay: Option<String>,
        /// Seconds to wait for LAN discovery
        #[arg(long, default_value_t = 3, value_name = "SECS")]
        wait: u64,
    },
    /// List devices on the LAN and in your tunnel group
    Devices {
        /// Seconds to wait for LAN discovery
        #[arg(long, default_value_t = 3, value_name = "SECS")]
        wait: u64,
        /// Relay URL for this run only
        #[arg(long, value_name = "WSS_URL")]
        relay: Option<String>,
    },
    /// Open the setup wizard (edge side, tunnel, pairing) — runs the app
    Setup,
    /// Show the current configuration and check the relay
    Status,
    /// Manage your tunnel group (create, join, approve members)
    #[command(subcommand)]
    Group(GroupCmd),
}

#[derive(Subcommand)]
pub enum GroupCmd {
    /// Create a new group on the relay — this device becomes its head
    Create,
    /// Ask to join a group; the head approves from their WhisperDrop
    Join {
        /// 6-digit group id
        group: String,
        /// Seconds to wait for the head's decision before returning
        #[arg(long, default_value_t = 60)]
        wait: u64,
    },
    /// Check a join request that is still pending
    Status,
    /// Head: list join requests waiting for a decision
    Pending,
    /// Head: approve a join request
    Approve {
        /// request id (from `group pending` or the tunnel log)
        request_id: String,
    },
    /// Head: deny a join request
    Deny { request_id: String },
    /// List the group's members
    Members,
    /// Head: remove a device from the group
    Kick { device_id: String },
    /// Leave the current group
    Leave,
}

fn is_group_id(s: &str) -> bool {
    s.len() == 6 && s.chars().all(|c| c.is_ascii_digit())
}

fn looks_like_ip(s: &str) -> bool {
    let host = s.split(':').next().unwrap_or("");
    host.parse::<std::net::Ipv4Addr>().is_ok()
}

fn fmt_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i > 0 && v < 10.0 {
        format!("{v:.1} {}", units[i])
    } else {
        format!("{} {}", v.round() as u64, units[i])
    }
}

/// Wait up to `secs` for LAN peers; returns early once `until` is satisfied.
async fn discover(secs: u64, until: impl Fn(&[crate::mdns::Peer]) -> bool) -> Vec<crate::mdns::Peer> {
    crate::mdns::browse_only();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let peers = crate::mdns::list();
        if until(&peers) || std::time::Instant::now() >= deadline {
            return peers;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// Pick the LAN target for `send`.
async fn resolve_target(to: Option<&str>, wait: u64) -> Result<crate::mdns::Peer, String> {
    match to {
        Some(t) if looks_like_ip(t) => {
            let mut parts = t.splitn(2, ':');
            let ip = parts.next().unwrap_or("").to_string();
            if let Some(port) = parts.next().and_then(|p| p.parse::<u16>().ok()) {
                return Ok(crate::mdns::Peer { name: ip.clone(), ip, port });
            }
            crate::sender::resolve(&ip).await
        }
        Some(name) => {
            let wanted = name.to_lowercase();
            let peers = discover(wait, |ps| ps.iter().any(|p| p.name.to_lowercase() == wanted)).await;
            peers
                .into_iter()
                .find(|p| p.name.to_lowercase() == wanted)
                .ok_or_else(|| format!("no device named \"{name}\" found on the LAN (try `whisperdrop devices`)"))
        }
        None => {
            let peers = discover(wait, |ps| ps.len() > 1).await;
            match peers.len() {
                1 => Ok(peers.into_iter().next().unwrap()),
                0 => Err("no devices on the LAN — pass --to <name|ip|group>, or --tunnel".into()),
                _ => {
                    let names: Vec<String> = peers.iter().map(|p| p.name.clone()).collect();
                    Err(format!("several devices found — pick one with --to: {}", names.join(", ")))
                }
            }
        }
    }
}

pub async fn run(cmd: Command) -> Result<(), String> {
    match cmd {
        Command::Run | Command::Setup => unreachable!("handled in main"),
        Command::Send { files, to, tunnel, secret, relay, wait } => {
            for f in &files {
                if !f.is_file() {
                    return Err(format!("not a file: {}", f.display()));
                }
            }
            let mut cfg = crate::config::load();
            if let Some(s) = secret {
                cfg.tunnel.shared_secret = s;
            }
            let relay = relay.unwrap_or_else(|| cfg.tunnel.relay.clone());
            *crate::RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());

            let group_target = to.as_deref().filter(|t| is_group_id(t)).map(str::to_string);
            if tunnel || group_target.is_some() {
                let group = group_target.unwrap_or_else(|| cfg.group_id.clone());
                if cfg.group_id.is_empty() || cfg.tunnel.member_token.is_empty() {
                    return Err("not in a tunnel group — `whisperdrop group create` or `whisperdrop group join <id>` first".into());
                }
                if group != cfg.group_id {
                    return Err(format!("this device is a member of group {} only — join {group} first", cfg.group_id));
                }
                if cfg.tunnel.shared_secret.is_empty() {
                    eprintln!("warning: no pairing passphrase — the transfer will not be encrypted");
                }
                crate::tunnel::set_group(group.clone());
                println!("→ group {group} via {relay}");
                for f in &files {
                    let path = f.display().to_string();
                    match crate::tunnel::send_over_tunnel(&relay, &group, &path).await {
                        Ok(n) => println!("✓ {} — {} sent, confirmed by the receiver", f.display(), fmt_bytes(n)),
                        Err(e) => return Err(format!("{}: {e}", f.display())),
                    }
                }
                return Ok(());
            }

            let peer = resolve_target(to.as_deref(), wait).await?;
            println!("→ {} ({}:{})", peer.name, peer.ip, peer.port);
            for f in &files {
                let path = f.display().to_string();
                match crate::sender::send_file(&peer.ip, peer.port, &path).await {
                    Ok(n) => println!("\r✓ {} — {} sent to {}          ", f.display(), fmt_bytes(n), peer.name),
                    Err(e) => return Err(format!("{}: {e}", f.display())),
                }
            }
            Ok(())
        }
        Command::Devices { wait, relay } => {
            let cfg = crate::config::load();
            *crate::RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
            let peers = discover(wait, |_| false).await;
            println!("LAN ({}):", peers.len());
            for p in &peers {
                println!("  {:<28} {}:{}", p.name, p.ip, p.port);
            }
            if peers.is_empty() {
                println!("  (none — is WhisperDrop running on the other machine?)");
            }
            if cfg.tunnel.enabled {
                let relay = relay.unwrap_or_else(|| cfg.tunnel.relay.clone());
                if cfg.group_id.is_empty() {
                    println!("tunnel: enabled but no group id — run `whisperdrop setup`");
                } else {
                    crate::tunnel::set_group(cfg.group_id.clone());
                    match crate::tunnel::devices(&relay, &cfg.tunnel.device_id).await {
                        Ok(ids) => {
                            println!("tunnel group {} ({}):", cfg.group_id, ids.len());
                            for id in &ids {
                                println!("  {id}");
                            }
                            if ids.is_empty() {
                                println!("  (no other device online in the group)");
                            }
                        }
                        Err(e) => println!("tunnel: relay unreachable — {e}"),
                    }
                }
            } else {
                println!("tunnel: disabled");
            }
            Ok(())
        }
        Command::Group(g) => group(g).await,
        Command::Status => {
            let cfg = crate::config::load();
            *crate::RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
            println!("whisperdrop {}", env!("CARGO_PKG_VERSION"));
            println!("  config      : {}", crate::config::config_path().display());
            println!("  drop zone   : {} edge", cfg.position);
            println!("  receive dir : {}", cfg.receive_dir);
            println!("  device id   : {}", cfg.tunnel.device_id);
            if cfg.tunnel.enabled {
                println!("  tunnel      : enabled — {}", cfg.tunnel.relay);
                println!("  group       : {}", if cfg.group_id.is_empty() { "(not set)" } else { &cfg.group_id });
                println!("  passphrase  : {}", if cfg.tunnel.shared_secret.is_empty() { "not set (unencrypted)" } else { "set" });
                print!("  relay       : ");
                match crate::tunnel::check_relay(&cfg.tunnel.relay, &cfg.tunnel.device_id).await {
                    Ok(()) => println!("reachable ✓"),
                    Err(e) => println!("unreachable — {e}"),
                }
            } else {
                println!("  tunnel      : disabled");
            }
            Ok(())
        }
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "device".into())
}

async fn group(cmd: GroupCmd) -> Result<(), String> {
    let cfg = crate::config::load();
    *crate::RUNTIME_CFG.lock().unwrap() = Some(cfg.clone());
    crate::tunnel::set_group(cfg.group_id.clone());
    let relay = cfg.tunnel.relay.clone();
    let need_group = || -> Result<String, String> {
        if cfg.group_id.is_empty() || cfg.tunnel.member_token.is_empty() {
            Err("not in a group — `whisperdrop group create` or `whisperdrop group join <id>`".into())
        } else {
            Ok(cfg.group_id.clone())
        }
    };
    match cmd {
        GroupCmd::Create => {
            if !cfg.group_id.is_empty() && !cfg.tunnel.member_token.is_empty() {
                return Err(format!("already in group {} — `whisperdrop group leave` first", cfg.group_id));
            }
            let (group, token) = crate::tunnel::group_create(&relay, &cfg.tunnel.device_id, &hostname()).await?;
            crate::save_membership(&group, &token, "head")?;
            println!("✓ group {group} created — you are its head");
            println!("  On your other devices: whisperdrop group join {group}  (or Preferences → Join)");
            println!("  Approve them here with: whisperdrop group approve <request-id>  (see `group pending`)");
            Ok(())
        }
        GroupCmd::Join { group, wait } => {
            if group.len() != 6 || !group.chars().all(|c| c.is_ascii_digit()) {
                return Err("group id must be 6 digits".into());
            }
            println!("→ asking the head of group {group} to admit this device… (waiting up to {wait}s)");
            let outcome = crate::tunnel::group_join(&relay, &group, &cfg.tunnel.device_id, &hostname(), wait).await?;
            crate::finish_join(&group, &outcome)?;
            report_join(&outcome)
        }
        GroupCmd::Status => {
            if cfg.tunnel.pending_request.is_empty() {
                if cfg.tunnel.member_token.is_empty() {
                    println!("not in a group");
                } else {
                    println!("in group {} as {}", cfg.group_id, cfg.tunnel.role);
                }
                return Ok(());
            }
            let outcome = crate::tunnel::group_join_status(&relay, &cfg.group_id, &cfg.tunnel.pending_request).await?;
            crate::finish_join(&cfg.group_id, &outcome)?;
            report_join(&outcome)
        }
        GroupCmd::Pending => {
            let g = need_group()?;
            let v = crate::tunnel::group_query(&relay, &g, serde_json::json!({"type":"pending"}), &["pending"]).await?;
            let list = v["requests"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("no join requests waiting");
            }
            for r in list {
                println!("{}  {:<24} {}  → whisperdrop group approve {}", r["request_id"].as_str().unwrap_or(""), r["device_name"].as_str().unwrap_or(""), r["device_id"].as_str().unwrap_or(""), r["request_id"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        GroupCmd::Approve { request_id } => decide(&relay, &need_group()?, &request_id, true).await,
        GroupCmd::Deny { request_id } => decide(&relay, &need_group()?, &request_id, false).await,
        GroupCmd::Members => {
            let g = need_group()?;
            let v = crate::tunnel::group_query(&relay, &g, serde_json::json!({"type":"members"}), &["members"]).await?;
            println!("group {g}:");
            for m in v["members"].as_array().cloned().unwrap_or_default() {
                println!("  {} {:<24} {}  {}", if m["online"].as_bool().unwrap_or(false) { "●" } else { "○" }, m["name"].as_str().unwrap_or(""), m["device_id"].as_str().unwrap_or(""), m["role"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        GroupCmd::Kick { device_id } => {
            let g = need_group()?;
            crate::tunnel::group_query(&relay, &g, serde_json::json!({"type":"kick","device_id":device_id}), &["ok"]).await?;
            println!("✓ {device_id} removed from group {g}");
            Ok(())
        }
        GroupCmd::Leave => {
            if let Ok(g) = need_group() {
                let _ = crate::tunnel::group_query(&relay, &g, serde_json::json!({"type":"leave"}), &["ok"]).await;
            }
            crate::leave_group()?;
            println!("✓ left the group");
            Ok(())
        }
    }
}

async fn decide(relay: &str, group: &str, request_id: &str, approved: bool) -> Result<(), String> {
    crate::tunnel::group_query(relay, group, serde_json::json!({"type":"approve","request_id":request_id,"approved":approved}), &["ok"]).await?;
    println!("✓ request {request_id} {}", if approved { "approved" } else { "denied" });
    Ok(())
}

fn report_join(outcome: &crate::tunnel::JoinOutcome) -> Result<(), String> {
    match outcome {
        crate::tunnel::JoinOutcome::Approved { .. } => {
            println!("✓ approved — this device is now a member");
            Ok(())
        }
        crate::tunnel::JoinOutcome::Denied => Err("the group head denied this device".into()),
        crate::tunnel::JoinOutcome::Pending { request_id } => {
            println!("⏳ still waiting for the head (request {request_id}) — run `whisperdrop group status` later");
            Ok(())
        }
    }
}
