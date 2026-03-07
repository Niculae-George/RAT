use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum SentinelPacket {
    Handshake { hostname: String, os: String, version: String },
    Command(String),
    Success(String),
    Error(String),
    Heartbeat,
    Disconnect,
}

// ── Framing helpers ──────────────────────────────────────────────────────────
// Wire format:  [ 4-byte little-endian length ][ bincode payload ]
// This prevents partial-read corruption when TCP segments are split.

pub fn encode_packet(packet: &SentinelPacket) -> Vec<u8> {
    let payload = bincode::serialize(packet).expect("serialize failed");
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&payload);
    buf
}

/// Read exactly `n` bytes from an async reader.
async fn read_exact_bytes<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
    n: usize,
) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut pos = 0;
    while pos < n {
        let read = reader.read(&mut buf[pos..]).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        pos += read;
    }
    Ok(buf)
}

pub async fn send_packet<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    packet: &SentinelPacket,
) -> std::io::Result<()> {
    let bytes = encode_packet(packet);
    writer.write_all(&bytes).await
}

pub async fn recv_packet<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<SentinelPacket> {
    let len_buf = read_exact_bytes(reader, 4).await?;
    let len = u32::from_le_bytes(len_buf.try_into().unwrap()) as usize;
    let payload = read_exact_bytes(reader, len).await?;
    bincode::deserialize(&payload).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })
}
