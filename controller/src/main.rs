use common::{recv_packet, send_packet, SentinelPacket};
use tokio::net::TcpListener;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use std::io::{self, Write};

const LISTEN_ADDR: &str = "0.0.0.0:8080";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    println!("╔══════════════════════════════════════╗");
    println!("║   Sentinel Controller  │  port 8080  ║");
    println!("╚══════════════════════════════════════╝");
    println!("Waiting for agent connections...\n");

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("[+] Incoming connection from {addr}");

        // Split the socket so we can read and write independently.
        let (mut reader, mut writer) = tokio::io::split(socket);

        // Expect a Handshake first.
        match recv_packet(&mut reader).await {
            Ok(SentinelPacket::Handshake { hostname, os, version }) => {
                println!("┌─ Agent connected ──────────────────────");
                println!("│  Host   : {hostname}");
                println!("│  OS     : {os}");
                println!("│  Version: {version}");
                println!("└────────────────────────────────────────");
                println!("Commands: :ping  :quit  :exit");
                println!();
            }
            Ok(other) => {
                eprintln!("[!] Expected Handshake, got {other:?}. Closing.");
                continue;
            }
            Err(e) => {
                eprintln!("[!] Failed to read handshake: {e}");
                continue;
            }
        }

        // Channel: background reader → command loop.
        // The reader task forwards every incoming packet so the command loop
        // can handle unsolicited heartbeats and detect dead connections even
        // while we are blocked waiting for stdin input.
        let (tx, mut rx) = mpsc::channel::<SentinelPacket>(16);
        let tx_err = tx.clone();
        tokio::spawn(async move {
            loop {
                match recv_packet(&mut reader).await {
                    Ok(pkt) => {
                        if tx.send(pkt).await.is_err() { break; }
                    }
                    Err(_) => {
                        // Signal dead connection by dropping the sender.
                        drop(tx_err);
                        break;
                    }
                }
            }
        });

        // Command loop — runs on the main thread so stdin works normally.
        'session: loop {
            print!("sentinel> ");
            io::stdout().flush().unwrap();

            // Drain any unsolicited packets that arrived while we were idle
            // (keepalive heartbeats, etc.) before blocking on stdin.
            while let Ok(pkt) = rx.try_recv() {
                match pkt {
                    SentinelPacket::Heartbeat => {} // silent — keepalive noise
                    other => eprintln!("[agent] unexpected packet: {other:?}"),
                }
            }

            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_err() {
                break;
            }
            let input = input.trim();
            if input.is_empty() { continue; }

            match input {
                ":ping" => {
                    if let Err(e) = send_packet(&mut writer, &SentinelPacket::Heartbeat).await {
                        eprintln!("[!] Send error: {e}");
                        break 'session;
                    }
                    // Wait up to 5 s for the pong.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        async {
                            loop {
                                match rx.recv().await {
                                    Some(SentinelPacket::Heartbeat) => return true,
                                    Some(_) => continue,
                                    None => return false,
                                }
                            }
                        },
                    )
                    .await
                    {
                        Ok(true) => println!("[+] Agent is alive."),
                        _ => println!("[!] No response — agent may be dead."),
                    }
                }
                ":quit" => {
                    let _ = send_packet(&mut writer, &SentinelPacket::Disconnect).await;
                    let _ = writer.shutdown().await;
                    println!("[*] Disconnected. Waiting for next agent...");
                    break 'session;
                }
                ":exit" => {
                    let _ = send_packet(&mut writer, &SentinelPacket::Disconnect).await;
                    let _ = writer.shutdown().await;
                    println!("[*] Shutting down.");
                    return Ok(());
                }
                cmd => {
                    if let Err(e) = send_packet(&mut writer, &SentinelPacket::Command(cmd.to_string())).await {
                        eprintln!("[!] Send error: {e}");
                        break 'session;
                    }
                    // Wait for the command response, skipping keepalive packets.
                    loop {
                        match rx.recv().await {
                            Some(SentinelPacket::Success(out)) => { println!("{out}"); break; }
                            Some(SentinelPacket::Error(err))   => { eprintln!("[err] {err}"); break; }
                            Some(SentinelPacket::Heartbeat)    => {} // skip keepalive noise
                            Some(other) => { eprintln!("[!] Unexpected: {other:?}"); break; }
                            None => {
                                eprintln!("[!] Connection lost.");
                                break 'session;
                            }
                        }
                    }
                }
            }
        }
    }
}
