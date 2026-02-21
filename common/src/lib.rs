use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum SentinelPacket {
    Handshake {
        hostname: String,
        version: String
    },
    Success(String),
    Error(String),
    Command(String),
}
