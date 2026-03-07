# Sentinel — Remote Access Tool

> **Status:** Working prototype — command execution, automatic reconnect, keepalive, no console window on target.

A personal remote access tool written in **Rust**, built as a systems-programming learning project.  
Allows opening a remote shell into a home Windows PC from any network, with no visible window on the target machine.

**Scope** — shell command execution only. No file transfer, no screen capture, no privilege escalation.

### Why Rust?
- Zero-cost async via `tokio` — the agent idles at near-zero CPU while waiting for commands
- Memory safety — no use-after-free or buffer overflows by construction
- Single statically-linked binary — no runtime dependencies to install on the target PC
- Cross-compilation — build `agent.exe` on macOS or Linux with one command

---

## Project Structure

```
RAT/
├── Cargo.toml               # Workspace root + shared release profile
├── Cargo.lock               # Pinned dependency tree
│
├── common/                  # Shared library — protocol types and framing
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs           # SentinelPacket enum, encode/decode, send/recv helpers
│
├── agent/                   # Target-side binary (runs on the home Windows PC)
│   ├── Cargo.toml
│   └── src/
│       └── main.rs          # Connect loop, keepalive, session handler, cmd execution
│
└── controller/              # Operator-side binary (runs on your laptop/desktop)
    ├── Cargo.toml
    └── src/
        └── main.rs          # TCP listener, handshake display, interactive shell prompt
```

### Crate responsibilities

| Crate | Type | Role |
|---|---|---|
| `common` | `lib` | Defines `SentinelPacket`, the 4-byte length-prefixed wire framing, and the async `send_packet` / `recv_packet` helpers used by both binaries |
| `agent` | `bin` | Reverse-connects to the controller, authenticates with a handshake, executes incoming commands via `cmd.exe`, sends back stdout/stderr, and handles keepalive + reconnect |
| `controller` | `bin` | Binds a TCP listener, accepts one agent at a time, displays the handshake info, and provides an interactive `sentinel>` prompt for sending commands and built-ins |

### Network topology

```
  ┌────────────────────────────────────────────────────┐
  │  Home network (dynamic IP / DDNS)                  │
  │                                                    │
  │  ┌──────────────┐          ┌──────────────────┐   │
  │  │  agent.exe   │──TCP────>│  Router / NAT    │   │
  │  │  (Windows)   │          │  port-forward    │   │
  │  └──────────────┘          │  8080 → PC       │   │
  │                            └──────────────────┘   │
  └────────────────────────────────────────────────────┘
                        │  public internet
  ┌─────────────────────▼──────────────────────────────┐
  │  University / any network                          │
  │                                                    │
  │  ┌──────────────────┐                              │
  │  │  controller      │  sentinel> dir /b            │
  │  │  (macOS / Linux) │  sentinel> ipconfig          │
  │  └──────────────────┘                              │
  └────────────────────────────────────────────────────┘
```

> The agent always **connects outward** (reverse connection). This means the home PC does not need to accept inbound connections — only the controller needs a reachable address. A single port-forward on the home router is still required so the controller can be reached from outside.

Wait — actually the controller is the one that **listens**. The agent dials out to wherever the controller is running. So it is the controller machine (university laptop, VPS, etc.) that needs a reachable IP and open port. The home PC needs no inbound port-forward at all.

---

## How It Works — Step by Step

### 1. Controller binds and waits
```
TcpListener::bind("0.0.0.0:8080")
```
The controller opens a TCP listener on all interfaces, port 8080, and blocks on `listener.accept()`.  
It accepts **one agent at a time** — when a session ends it loops back and waits for the next one.

---

### 2. Agent resolves the controller address and connects
```rust
const CONTROLLER_ADDR: &str = "your.ddns.hostname:8080";
```
Tokio resolves the address (DNS lookup if it's a hostname, no-op if it's a plain IP) and calls `TcpStream::connect`.  
If the controller is unreachable the agent does **not** crash — it enters the backoff loop (see step 6).

---

### 3. TCP keepalive is configured on the socket
Before any data is sent, `socket2` is used to configure OS-level TCP keepalive directly on the file descriptor:

```rust
let keepalive = socket2::TcpKeepalive::new()
    .with_time(Duration::from_secs(30))   // start probing after 30 s of silence
    .with_interval(Duration::from_secs(10)) // re-probe every 10 s
    .with_retries(3);                       // give up after 3 missed probes (~1 min)
```

This is distinct from the application-level heartbeat (step 5). The OS keepalive catches truly silent dead connections (e.g. the controller process crashes without sending a FIN) that the application layer would never see.

---

### 4. Handshake
The very first packet the agent sends is a `Handshake`:

```rust
SentinelPacket::Handshake {
    hostname: whoami::fallible::hostname()?,   // e.g. "DESKTOP-ABC123"
    os:       "Windows Windows 11 Home",
    version:  "0.1.0",                         // from Cargo.toml
}
```

The controller reads this, validates that it is indeed a `Handshake` variant (rejects anything else), prints the info, and enters the command loop:

```
┌─ Agent connected ──────────────────────
│  Host   : DESKTOP-ABC123
│  OS     : Windows Windows 11 Home
│  Version: 0.1.0
└────────────────────────────────────────
```

---

### 5. Application-level keepalive heartbeats
The agent runs a `tokio::time::interval` timer alongside the packet receiver using `tokio::select!`:

```rust
tokio::select! {
    packet = recv_packet(stream) => { /* handle command */ }
    _ = keepalive_tick.tick()   => {
        send_packet(stream, &SentinelPacket::Heartbeat).await?;
    }
}
```

Every **30 seconds** of idleness the agent proactively sends a `Heartbeat` packet.  
Purpose: keeps NAT table entries alive. Most home routers drop UDP/TCP state after 30–120 s of silence. Without heartbeats a connection that looks open on both ends is silently dead at the router level — the next command would hang forever.

The controller receives these on a **background reader task** (a `tokio::spawn`) that forwards all packets into an `mpsc` channel. Heartbeats are silently discarded so they never appear at the `sentinel>` prompt.

---

### 6. Exponential backoff reconnect
Every failed connection attempt doubles the wait before the next retry:

```
attempt 1 → wait  5 s
attempt 2 → wait 10 s
attempt 3 → wait 20 s
attempt 4 → wait 40 s
attempt 5 → wait 80 s
attempt 6+ → wait 120 s  (capped)
```

On a successful connection the delay resets to 5 s.  
This means the agent is responsive (retries quickly) after a short outage, but backs off gracefully if the controller is down for a long time — avoiding pointless network chatter.

---

### 7. Controller: split socket + background reader
After accepting a connection the controller splits the `TcpStream` into independent halves:

```rust
let (mut reader, mut writer) = tokio::io::split(socket);
```

A `tokio::spawn` task owns `reader` and forwards every packet into an `mpsc::channel`. The main thread owns `writer` and blocks on `stdin::read_line`.  

This solves a classic async problem: if you block the thread on stdin you can't simultaneously read from the socket. With the channel in between:
- Unsolicited packets (heartbeats) are drained silently before each prompt
- A dead connection is detected the moment the background task gets an EOF — `rx.recv()` returns `None` in the main thread
- `:ping` can wait for a response with a timeout without blocking stdin for other inputs

---

### 8. Command execution
Commands are run on a `tokio::task::spawn_blocking` thread (Tokio's blocking thread pool) so the async executor is never stalled by a long-running child process:

```rust
Command::new("cmd").args(["/C", &cmd_str]).output()
```

- `stdout` → `SentinelPacket::Success(String)`
- `stderr` / non-zero exit → `SentinelPacket::Error(String)`
- spawn failure → `SentinelPacket::Error("Failed to execute command")`

The full stdout/stderr is captured and sent back as a single packet. Commands that produce large output (e.g. `dir /s`) are fine up to the limits of available memory.

---

### 9. Clean disconnect
`:quit` or `:exit` in the controller sends `SentinelPacket::Disconnect`.  
The agent receives it, breaks the session loop, and immediately re-enters the outer reconnect loop — ready to dial in again if the controller comes back up.  
The controller calls `writer.shutdown()` to send a TCP FIN before looping back to `listener.accept()`.

---

## Wire Protocol

### Frame layout

```
Byte offset   0        1        2        3        4 … (4+N-1)
              ┌────────┬────────┬────────┬────────┬──────────────────────┐
              │  len   │  len   │  len   │  len   │  bincode payload     │
              │ [0]    │ [1]    │ [2]    │ [3]    │  N bytes             │
              └────────┴────────┴────────┴────────┴──────────────────────┘
              └──────── u32 little-endian ────────┘
```

- **Length prefix** — 4-byte unsigned 32-bit integer, little-endian. Represents the number of bytes in the payload only (does not include the 4 bytes of the prefix itself).
- **Payload** — a `bincode`-serialised `SentinelPacket` enum variant. `bincode` uses a compact binary format: enum discriminants are encoded as `u32`, strings as `u64` length + UTF-8 bytes.
- **Max packet size** — `u32::MAX` (~4 GB) theoretical limit. In practice the largest packet is a `Success` or `Error` containing a command's full stdout, bounded by available RAM.

### Why length-prefixed framing?
TCP is a **stream** protocol — it provides no message boundaries. A single `socket.write_all(bytes)` on one side may be received as multiple `read()` calls on the other, or multiple writes may be coalesced into one read. Without framing, `bincode::deserialize` would silently corrupt on any split read.

The `recv_packet` helper loops until exactly `N` bytes have been read before handing them to `bincode`:

```rust
// pseudocode
let len   = read_exact(reader, 4).await?;   // always 4 bytes
let body  = read_exact(reader, len).await?; // always exactly len bytes
bincode::deserialize(&body)
```

### Packet variants

| Variant | Direction | Payload | Purpose |
|---|---|---|---|
| `Handshake { hostname, os, version }` | agent → controller | 3 strings | First packet on connect — identifies the agent |
| `Command(String)` | controller → agent | shell command string | Run via `cmd /C` |
| `Success(String)` | agent → controller | stdout string | Command completed successfully |
| `Error(String)` | agent → controller | stderr / error string | Command failed or couldn't start |
| `Heartbeat` | both directions | *(empty)* | Keepalive probe or pong reply |
| `Disconnect` | controller → agent | *(empty)* | Graceful session termination |

---

## Controller Built-in Commands

| Input | Action |
|---|---|
| Any text | Forwarded as `cmd /C <text>` to the agent; stdout printed, stderr prefixed with `[err]` |
| `:ping` | Sends a `Heartbeat` packet, waits up to **5 s** for a `Heartbeat` reply, reports alive/dead |
| `:quit` | Sends `Disconnect`, shuts down the writer half, loops back to wait for the next agent |
| `:exit` | Sends `Disconnect`, shuts down the writer half, terminates the controller process |

### Example session

```
╔══════════════════════════════════════╗
║   Sentinel Controller  │  port 8080  ║
╚══════════════════════════════════════╝
Waiting for agent connections...

[+] Incoming connection from 203.0.113.42:51234
┌─ Agent connected ──────────────────────
│  Host   : DESKTOP-ABC123
│  OS     : Windows Windows 11 Home
│  Version: 0.1.0
└────────────────────────────────────────
Commands: :ping  :quit  :exit

sentinel> whoami
desktop-abc123\george

sentinel> dir /b C:\Users\george\Desktop
notes.txt
project.zip

sentinel> :ping
[+] Agent is alive.

sentinel> :quit
[*] Disconnected. Waiting for next agent...
```

---

## Async Architecture

Both binaries are built on **Tokio**, Rust's most widely used async runtime. Understanding the async model helps explain several design decisions.

### Tokio runtime (`#[tokio::main]`)
The `#[tokio::main]` macro expands to a multi-threaded Tokio runtime. All `async fn` calls are compiled to state machines and driven by the runtime's executor — no OS thread is blocked while waiting for I/O.

### Why `spawn_blocking` for command execution
`std::process::Command::output()` is a **blocking** system call — it blocks the calling OS thread until the child process exits. Calling it directly inside an `async fn` would stall the Tokio executor thread, preventing all other async tasks (including heartbeats and socket reads) from making progress. Wrapping it in `task::spawn_blocking` moves it onto a dedicated blocking thread pool:

```rust
let result = task::spawn_blocking(move || {
    Command::new("cmd").args(["/C", &cmd_str]).output()
}).await;
```

### Why `tokio::select!` in the agent
The agent needs to do two things simultaneously while waiting for a command:
1. Receive the next packet from the controller
2. Fire a heartbeat every 30 seconds

`tokio::select!` polls both futures concurrently and runs whichever completes first:

```rust
tokio::select! {
    packet = recv_packet(stream) => { /* got a packet */ }
    _ = keepalive_tick.tick()   => { /* 30 s elapsed — send heartbeat */ }
}
```

### Why `tokio::io::split` in the controller
A `TcpStream` cannot be shared across threads without synchronisation. `tokio::io::split` divides it into a `ReadHalf` and a `WriteHalf` — each can be moved into a different task independently:

```
TcpStream
  ├─ ReadHalf  ──→ background tokio::spawn task  (forwards packets to mpsc channel)
  └─ WriteHalf ──→ main thread                   (sends commands from stdin)
```

This lets the controller detect a broken connection (EOF on `ReadHalf`) even while the main thread is blocked in `io::stdin().read_line()`.

---

## Setup & Deployment

### Prerequisites
- Rust toolchain (`rustup`)
- `x86_64-pc-windows-gnu` target for cross-compiling the agent from macOS/Linux:
  ```bash
  rustup target add x86_64-pc-windows-gnu
  brew install mingw-w64   # macOS only
  ```
- Port **8080** open/forwarded on the machine running the controller
- Optionally a free DDNS hostname ([DuckDNS](https://www.duckdns.org), [No-IP](https://www.noip.com)) if your controller IP is dynamic

### 1. Set the controller address
Edit `agent/src/main.rs` before building:
```rust
// Use a DDNS hostname if your controller IP changes:
const CONTROLLER_ADDR: &str = "your.ddns.hostname:8080";

// Or a plain IP if it's static:
const CONTROLLER_ADDR: &str = "203.0.113.42:8080";
```

### 2. Build the controller
The controller runs on your machine (macOS, Linux, or Windows):
```bash
cargo build --release -p controller
# binary: target/release/controller  (or controller.exe on Windows)
```

### 3. Cross-compile the agent for Windows
```bash
cargo build --release --target x86_64-pc-windows-gnu -p agent
# binary: target/x86_64-pc-windows-gnu/release/agent.exe
```

In **release** mode the `windows_subsystem = "windows"` PE flag is active — **no console window appears** on the target PC.  
In **debug** mode (`cargo build`) the console is visible, which is useful during development.

### 4. Run

**On your machine (controller) — start this first:**
```bash
./target/release/controller
# Windows:
.\target\release\controller.exe
```

**On the target Windows PC:**  
Copy `agent.exe` over (USB, shared folder, etc.) and run it once. It connects back to the controller automatically and reconnects whenever the session drops.

### 5. Port setup
The controller needs to be reachable on port 8080 from the internet (or whatever network the agent is on):

| Scenario | What to do |
|---|---|
| Controller on your university laptop | Make sure the university firewall allows inbound TCP 8080. Some networks block it — try port 443 as a fallback. |
| Controller on a home router | Add a port-forward rule: external TCP 8080 → laptop LAN IP:8080 |
| Controller on a VPS | Open TCP 8080 in the VPS firewall/security group |

### 6. DDNS setup (recommended)
If your controller machine has a dynamic IP (most home and university connections do):

1. Register a free account at [DuckDNS](https://www.duckdns.org)
2. Create a subdomain, e.g. `sentinel-ctrl.duckdns.org`
3. Install the DuckDNS update client on your controller machine so it updates the DNS record whenever your IP changes
4. Set `CONTROLLER_ADDR` in the agent to `sentinel-ctrl.duckdns.org:8080`

### Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Agent retries every N seconds, never connects | Controller not running, wrong IP/port, firewall blocking | Verify controller is running, check port-forward, try `telnet <ip> 8080` |
| Controller shows `[+] Incoming connection` but then immediately `[!] Failed to read handshake` | Agent version mismatch, corrupted binary | Rebuild both binaries from the same commit |
| Commands hang and never return output | NAT table dropped the connection silently | Reduce `KEEPALIVE_INTERVAL_SECS` or check router keepalive timeout settings |
| `cargo build --target x86_64-pc-windows-gnu` fails with linker error | `mingw-w64` not installed | `brew install mingw-w64` (macOS) or `sudo apt install gcc-mingw-w64` (Linux) |

---

## Dependencies

| Crate | Version | Used in | Purpose |
|---|---|---|---|
| `tokio` | 1.28 | all | Async runtime, TCP sockets, timers, channels, task spawning |
| `serde` | 1.0 | common, agent | Derive macros for `Serialize` / `Deserialize` on `SentinelPacket` |
| `bincode` | 1.3 | common | Compact binary serialisation of `SentinelPacket` variants |
| `socket2` | 0.5 | agent | Low-level socket options — used to set OS-level TCP keepalive parameters |
| `whoami` | 1.5 | agent | Cross-platform hostname and OS string detection for the handshake |

---

## Future Improvements

### 1. TLS encryption (`rustls`)
All traffic is currently **plaintext** over TCP. Anyone on the network path (ISP, university Wi-Fi, VPN exit node) can read every command and its output with a simple packet capture.

Fix: wrap the `TcpStream` with `tokio-rustls`. The controller generates a self-signed certificate at startup; the agent is compiled with the certificate's public key baked in and verifies it on connect. This gives full encryption with zero external CA dependency.

```toml
# Add to both agent and controller:
tokio-rustls = "0.26"
rustls = "0.23"
```

### 2. Shared-secret authentication
Currently any TCP client that speaks the `SentinelPacket` format and sends a valid `Handshake` is accepted. There is no way to distinguish your agent from someone else's client.

Fix: add a `token: String` field to `Handshake`. The token is a SHA-256 HMAC of a pre-shared secret + a timestamp, so it can't be replayed. The controller rejects any handshake with a wrong or missing token.

### 3. Multi-agent support
The controller currently handles **one agent at a time** — it blocks on `listener.accept()` and only loops back after the session ends. If two agents connect simultaneously, the second one waits in the TCP accept backlog.

Fix: spawn a `tokio::task` per accepted connection and maintain a `HashMap<String, SessionHandle>` keyed by hostname. Add controller commands:
- `:list` — print all active sessions
- `:switch <hostname>` — set the active session
- `:broadcast <cmd>` — send a command to all agents

### 4. PowerShell support
`cmd /C` is the legacy Windows shell. Most modern Windows administration uses PowerShell, which has access to .NET types, WMI, the registry, and a vastly richer scripting environment.

Fix: add a `Shell` variant to `SentinelPacket`:
```rust
Shell { interpreter: ShellType, command: String }
// ShellType: Cmd | PowerShell | PowerShellCore
```
Or simpler: a `:ps <cmd>` prefix in the controller that routes to `powershell.exe -NonInteractive -Command`.

### 5. Runtime-configurable address
The agent's `CONTROLLER_ADDR` is a compile-time constant. Changing it requires a recompile and re-deployment.

Fix: on startup, check for a config file (e.g. `sentinel.toml` next to the executable) and fall back to the compiled-in default. A minimal TOML config:

```toml
controller = "sentinel-ctrl.duckdns.org:8080"
reconnect_min_secs = 5
reconnect_max_secs = 120
keepalive_secs = 30
```

### 6. Structured session logging
Currently command history only exists in the terminal scrollback buffer — it's lost when the controller window closes.

Fix: write a structured log file (JSON Lines) with one entry per command:
```json
{"ts":"2026-03-07T14:23:01Z","host":"DESKTOP-ABC123","cmd":"whoami","exit":0,"stdout":"desktop-abc123\\george\r\n","duration_ms":47}
```
This gives a full audit trail and makes it easy to grep past sessions.

### 7. Windows startup persistence (registry `Run` key)
Add a `:startup on` controller command that tells the agent to register itself in the Windows registry so it starts automatically after a reboot — without needing to manually run `agent.exe` again.

```rust
// On the agent side, when it receives a future SetStartup { enable } packet:
use winreg::{enums::HKEY_CURRENT_USER, RegKey};
let hkcu = RegKey::predef(HKEY_CURRENT_USER);
let run = hkcu.open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_WRITE)?;
run.set_value("Sentinel", &std::env::current_exe()?.to_string_lossy().as_ref())?;
```

Requires adding the `winreg` crate to `agent/Cargo.toml`.

### 8. Compressed output
Large command outputs (e.g. `dir /s C:\`, `netstat -an`) can produce tens of kilobytes of text. Compressing the `Success` payload with `flate2` (zlib/gzip) before sending could reduce bandwidth significantly on slow connections.

```toml
# Add to common:
flate2 = "1.0"
```

Compression would be applied transparently inside `encode_packet` / `decode_packet` when payload size exceeds a threshold (e.g. 1 KB).
