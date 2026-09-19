//! First-run setup wizard (console). Steps:
//! 1. local network   2. tunnel (riki-api.online)   3. position L/R
//! 4. firewall auto   5. summary with redo
//! Run automatically when config.wizard_done == false, or via --wizard.

use crate::config::Config;

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap_or(0);
    line.trim().to_string()
}

fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("10.255.255.255", 1)).ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

async fn step1_local() -> bool {
    println!("\n[1/4] Local network");
    match local_ip() {
        Some(ip) => {
            println!("   ✓ LAN IP: {ip}");
            // probe that our receive port range is free
            for port in [51731u16, 51730] {
                if tokio::net::TcpListener::bind(("0.0.0.0", port))
                    .await
                    .is_ok()
                {
                    println!("   ✓ port {port} available for receiving");
                    return true;
                }
            }
            println!("   ✗ ports 51730-51731 busy");
            false
        }
        None => {
            println!("   ✗ no LAN adapter found");
            false
        }
    }
}

async fn step2_tunnel(cfg: &mut Config) -> bool {
    println!(
        "\n[2/4] Tunnel — send across networks via {}",
        cfg.tunnel.relay
    );
    let ans = ask("   enable tunnel? (Y/n): ");
    let enable = !(ans.eq_ignore_ascii_case("n") || ans.eq_ignore_ascii_case("no"));
    cfg.tunnel.enabled = enable;
    if !enable {
        println!("   – tunnel disabled (LAN-only mode)");
        return true; // not a failure: user opted out
    }
    println!("   device identity: {}", cfg.tunnel.device_id);
    println!("   testing relay…");
    match crate::tunnel::devices(&cfg.tunnel.relay, &cfg.tunnel.device_id).await {
        Ok(ids) => {
            println!(
                "   ✓ relay reachable — {} device(s) online {ids:?}",
                ids.len()
            );
            true
        }
        Err(e) => {
            println!("   ✗ relay unreachable: {e}");
            println!("     (deploy the relay binary to riki-api.online, or fix the URL)");
            false
        }
    }
}

async fn step3_position(cfg: &mut Config) -> bool {
    println!("\n[3/4] Drop zone position");
    let ans = ask("   1) Left   2) Right   — choose (1/2): ");
    cfg.position = if ans == "1" {
        "left".into()
    } else {
        "right".into()
    };
    println!("   ✓ drop zone on the {}", cfg.position);
    true
}

async fn step4_firewall() -> bool {
    println!("\n[4/4] Firewall");
    #[cfg(windows)]
    {
        crate::firewall::ensure();
        let ok = crate::firewall::rule_exists();
        println!(
            "   {} inbound rule",
            if ok { "✓ present" } else { "✗ missing" }
        );
        ok
    }
    #[cfg(not(windows))]
    {
        println!("   ✓ not required on this platform");
        true
    }
}

pub async fn run(cfg: &mut Config) {
    println!("========================================");
    println!(" WhisperDrop Setup Wizard");
    println!("========================================");

    loop {
        let s1 = step1_local().await;
        let s2 = step2_tunnel(cfg).await;
        let s3 = step3_position(cfg).await;
        let s4 = step4_firewall().await;

        println!("\n========== Summary ==========");
        print!("  [{}] 1. local network ", i(s1));
        print!("[{}] 2. tunnel ", i(s2));
        println!("[{}] 3. position ({})", i(s3), cfg.position);
        println!("  [{}] 4. firewall", i(s4));
        println!("=============================");

        let all_ok = s1 && s2 && s3 && s4;
        if all_ok {
            break;
        }
        let failed: Vec<&str> = [(s1, "1"), (s2, "2"), (s3, "3"), (s4, "4")]
            .iter()
            .filter(|(ok, _)| !ok)
            .map(|(_, n)| *n)
            .collect();
        let ans = ask(&format!(
            "  step(s) {} failed — type a number to redo, or Enter to finish anyway: ",
            failed.join(",")
        ));
        if ans.is_empty() {
            break;
        }
        if ans == "1" {
            let _ = step1_local().await;
        } else if ans == "2" {
            let _ = step2_tunnel(cfg).await;
        } else if ans == "3" {
            let _ = step3_position(cfg).await;
        } else if ans == "4" {
            let _ = step4_firewall().await;
        }
        // re-evaluate summary on next loop only when redoing everything —
        // for simplicity, break after one redo pass
        break;
    }

    cfg.wizard_done = true;
    if let Err(e) = crate::config::save(cfg) {
        println!("(config save failed: {e})");
    } else {
        println!("setup saved ✓ — ready!");
    }
}

fn i(ok: bool) -> char {
    if ok {
        '✓'
    } else {
        '✗'
    }
}
