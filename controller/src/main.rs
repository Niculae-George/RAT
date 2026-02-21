use common::SentinelPacket;
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::main]
async fn  main() -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    println!("Controller listening on port 8080...");

    let (mut socket, addr) = listener.accept().await?;
    println!("Accepted connection from {}" ,addr);

    let mut buf = [0u8; 1024];
    let n = socket.read(&mut buf).await?;
    let handshake: SentinelPacket = bincode::deserialize(&buf[..n]).unwrap();
    println!("Linked to: {:?}", handshake);
    loop {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).expect("Failed to read");
        let packet = SentinelPacket::Command(input.trim().to_string());
        let bytes = bincode::serialize(&packet).unwrap();
        socket.write_all(&bytes).await?;
        // Wait for the response from the agent
        let mut res_buf = [0u8;4096];
        let n = socket.read(&mut res_buf).await?;
        let response: SentinelPacket = bincode::deserialize(&res_buf[..n]).unwrap();
        // Handle the response based on its type
        match response {
            SentinelPacket::Success(output) => println!("Command output:\n{}", output),
            SentinelPacket::Error(err) => println!("Command error:\n{}", err),
            _ => println!("Unexpected packet: {:?}", response),
        }


    }


}
