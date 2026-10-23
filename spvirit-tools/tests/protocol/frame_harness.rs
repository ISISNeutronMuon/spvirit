#![allow(dead_code)]

use std::net::{SocketAddr, TcpListener, TcpStream as StdTcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Locate a workspace binary by name.  Test binaries live in
/// `target/<profile>/deps/`, so we go up two levels to reach
/// `target/<profile>/` where regular binaries are placed.
pub fn workspace_bin(name: &str) -> String {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let test_exe = std::env::current_exe().expect("cannot locate test executable");
    test_exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("{name}{ext}"))
        .to_string_lossy()
        .to_string()
}

use spvirit_codec::epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvirit_encode::encode_header;
use spvirit_tools::spvirit_client::client::{
    build_client_validation, encode_create_channel_request,
};
use spvirit_tools::spvirit_client::types::PvGetOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

fn free_tcp_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|addr| addr.port())
}

fn free_udp_port() -> Option<u16> {
    UdpSocket::bind("127.0.0.1:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .map(|addr| addr.port())
}

pub fn write_test_db_file() -> std::io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "pva_protocol_{}_{}_{}.db",
        std::process::id(),
        seq,
        nanos
    ));
    let contents = r#"
record(ai, "SIM:AI") {
    field(VAL, "1.25")
}
record(ao, "SIM:AO") {
    field(VAL, "2.50")
}
record(bi, "SIM:BI") {
    field(VAL, "1")
}
record(bo, "SIM:BO") {
    field(VAL, "0")
}
record(stringin, "SIM:STRIN") {
    field(VAL, "hello")
}
record(stringout, "SIM:STROUT") {
    field(VAL, "world")
}
"#;
    std::fs::write(&path, contents)?;
    Ok(path)
}

pub struct TestServer {
    pub tcp_port: u16,
    pub udp_port: u16,
    db_path: PathBuf,
    child: Child,
}

impl TestServer {
    pub fn spawn() -> Result<Self, String> {
        Self::spawn_with_args(&[])
    }

    pub fn spawn_with_args(extra_args: &[&str]) -> Result<Self, String> {
        let tcp_port = free_tcp_port().ok_or_else(|| "failed to allocate tcp port".to_string())?;
        let udp_port = free_udp_port().ok_or_else(|| "failed to allocate udp port".to_string())?;
        let db_path = write_test_db_file().map_err(|e| format!("write temp db: {}", e))?;

        let server_bin = workspace_bin("spserver");
        let mut cmd = Command::new(server_bin);
        cmd.arg("--db-file")
            .arg(&db_path)
            .arg("--listen-addr")
            .arg("127.0.0.1")
            .arg("--tcp-port")
            .arg(tcp_port.to_string())
            .arg("--udp-port")
            .arg(udp_port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for arg in extra_args {
            cmd.arg(arg);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn spvirit_server: {}", e))?;

        let addr: SocketAddr = format!("127.0.0.1:{tcp_port}")
            .parse()
            .map_err(|e| format!("invalid server addr: {}", e))?;
        let start = std::time::Instant::now();
        let ready_timeout = Duration::from_secs(2);
        let mut ready = false;
        while start.elapsed() < ready_timeout {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("spvirit_server exited early: {}", status));
            }
            if StdTcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !ready {
            return Err(format!(
                "spvirit_server not ready on {} within {:?}",
                addr, ready_timeout
            ));
        }

        Ok(Self {
            tcp_port,
            udp_port,
            db_path,
            child,
        })
    }

    pub async fn connect(&self) -> Result<TestSession, String> {
        let addr = format!("127.0.0.1:{}", self.tcp_port);
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect tcp: {}", e))?;
        Ok(TestSession {
            stream,
            version: 2,
            is_be: false,
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.db_path);
    }
}

pub struct TestSession {
    stream: TcpStream,
    pub version: u8,
    pub is_be: bool,
}

impl TestSession {
    pub async fn read_raw_packet(&mut self, timeout_dur: Duration) -> Result<Vec<u8>, String> {
        let mut header = [0u8; 8];
        timeout(timeout_dur, self.stream.read_exact(&mut header))
            .await
            .map_err(|_| "timeout reading header".to_string())?
            .map_err(|e| format!("read header: {}", e))?;

        let parsed = PvaHeader::new(&header);
        let payload_len = if parsed.flags.is_control {
            0usize
        } else {
            parsed.payload_length as usize
        };

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            timeout(timeout_dur, self.stream.read_exact(&mut payload))
                .await
                .map_err(|_| "timeout reading payload".to_string())?
                .map_err(|e| format!("read payload: {}", e))?;
        }

        let mut full = header.to_vec();
        full.extend_from_slice(&payload);
        Ok(full)
    }

    pub async fn read_decoded_packet(
        &mut self,
        timeout_dur: Duration,
    ) -> Result<(Vec<u8>, PvaPacketCommand), String> {
        let full = self.read_raw_packet(timeout_dur).await?;
        let mut pkt = PvaPacket::new(&full);
        let cmd = pkt
            .decode_payload()
            .ok_or_else(|| "decode payload failed".to_string())?;
        Ok((full, cmd))
    }

    pub async fn write_packet(&mut self, payload: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(payload)
            .await
            .map_err(|e| format!("write packet: {}", e))
    }

    pub async fn handshake(&mut self) -> Result<(), String> {
        let timeout_dur = Duration::from_secs(2);
        let mut saw_validation = false;

        for _ in 0..2 {
            let raw = self.read_raw_packet(timeout_dur).await?;
            let header = PvaHeader::new(&raw);
            let mut pkt = PvaPacket::new(&raw);
            if let Some(cmd) = pkt.decode_payload() {
                match cmd {
                    PvaPacketCommand::Control(payload) if payload.command == 2 => {
                        self.is_be = header.flags.is_msb;
                    }
                    PvaPacketCommand::ConnectionValidation(_) => {
                        self.version = header.version;
                        self.is_be = header.flags.is_msb;
                        saw_validation = true;
                    }
                    _ => {}
                }
            }
        }

        if !saw_validation {
            return Err("did not receive server validation".to_string());
        }

        let opts = PvGetOptions::new("SIM:AI".to_string());
        let validation = build_client_validation(&opts, self.version, self.is_be);
        self.write_packet(&validation).await?;

        let deadline = tokio::time::Instant::now() + timeout_dur;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("timeout waiting for CONNECTION_VALIDATED".to_string());
            }
            let remain = deadline - now;
            let (_raw, cmd) = self.read_decoded_packet(remain).await?;
            if matches!(cmd, PvaPacketCommand::ConnectionValidated(_)) {
                return Ok(());
            }
        }
    }

    pub async fn create_channel(&mut self, cid: u32, pv: &str) -> Result<u32, String> {
        let req = encode_create_channel_request(cid, pv, self.version, self.is_be);
        self.write_packet(&req).await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("timeout waiting for CREATE_CHANNEL response".to_string());
            }
            let remain = deadline - now;
            let (_raw, cmd) = self.read_decoded_packet(remain).await?;
            if let PvaPacketCommand::CreateChannel(payload) = cmd
                && payload.is_server
                && payload.cid == cid
            {
                if let Some(status) = payload.status {
                    return Err(format!(
                        "create channel failed code={} msg={}",
                        status.code,
                        status.message.unwrap_or_default()
                    ));
                }
                return Ok(payload.sid);
            }
        }
    }

    pub async fn send_client_application(
        &mut self,
        command: u8,
        body: &[u8],
    ) -> Result<(), String> {
        let mut frame = encode_header(
            false,
            self.is_be,
            false,
            self.version,
            command,
            body.len() as u32,
        );
        frame.extend_from_slice(body);
        self.write_packet(&frame).await
    }

    pub async fn send_client_op(
        &mut self,
        command: u8,
        sid: u32,
        ioid: u32,
        subcmd: u8,
        body: &[u8],
    ) -> Result<(), String> {
        let mut payload = Vec::with_capacity(9 + body.len());
        payload.extend_from_slice(&if self.is_be {
            sid.to_be_bytes()
        } else {
            sid.to_le_bytes()
        });
        payload.extend_from_slice(&if self.is_be {
            ioid.to_be_bytes()
        } else {
            ioid.to_le_bytes()
        });
        payload.push(subcmd);
        payload.extend_from_slice(body);
        self.send_client_application(command, &payload).await
    }

    pub async fn send_get_field(
        &mut self,
        cid: u32,
        field_name: Option<&str>,
    ) -> Result<(), String> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&if self.is_be {
            cid.to_be_bytes()
        } else {
            cid.to_le_bytes()
        });
        if let Some(name) = field_name {
            payload.extend_from_slice(&encode_pva_size(name.len(), self.is_be));
            payload.extend_from_slice(name.as_bytes());
        }
        self.send_client_application(17, &payload).await
    }

    pub async fn read_until<F>(
        &mut self,
        timeout_dur: Duration,
        mut predicate: F,
    ) -> Result<PvaPacketCommand, String>
    where
        F: FnMut(&PvaPacketCommand) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout_dur;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("timeout waiting for expected command".to_string());
            }
            let remain = deadline - now;
            let (_raw, cmd) = self.read_decoded_packet(remain).await?;
            if predicate(&cmd) {
                return Ok(cmd);
            }
        }
    }
}

fn encode_pva_size(size: usize, is_be: bool) -> Vec<u8> {
    if size == 0 {
        return vec![0x00];
    }
    if size < 254 {
        return vec![size as u8];
    }
    let mut out = vec![0xFE];
    out.extend_from_slice(&if is_be {
        (size as u32).to_be_bytes()
    } else {
        (size as u32).to_le_bytes()
    });
    out
}
