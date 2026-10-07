use anyhow::{Result, ensure};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snow::resolvers::CryptoResolver;
use snow::{Builder, TransportState};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    time::Duration,
};

const NOISE: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const MAX_MESSAGE: usize = 8192;
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionIntent {
    Manual,
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    UserDisconnect,
    ReceiverClosed,
}

#[derive(Clone)]
pub struct Identity {
    private_key: Vec<u8>,
    id: String,
}
impl Identity {
    pub fn load(path: &Path) -> Result<Self> {
        let private_key = match fs::read(path) {
            Ok(key) => key,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let key = Builder::new(NOISE.parse()?).generate_keypair()?.private;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                file.write_all(&key)?;
                key
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(private_key.len() == 32, "invalid stored identity");
        // Stable discovery identifier; authenticated peer identity comes from Noise.
        let id = hex::encode(Sha256::digest(&private_key))[..16].to_owned();
        Ok(Self { private_key, id })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn fingerprint(&self) -> String {
        let mut dh = snow::resolvers::DefaultResolver
            .resolve_dh(&snow::params::DHChoice::Curve25519)
            .expect("Noise Curve25519 resolver");
        dh.set(&self.private_key);
        hex::encode(Sha256::digest(dh.pubkey()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Message {
    Hello {
        name: String,
        protocol: u32,
        #[serde(default)]
        control_port: Option<u16>,
    },
    Pending {
        code: String,
    },
    Request {
        intent: ConnectionIntent,
    },
    Ready,
    Accept {
        key: [u8; 32],
        audio_port: u16,
        buffer_ms: u64,
    },
    Reject {
        reason: String,
    },
    Ping {
        serial: u64,
        rtt_ms: Option<f64>,
        capture_period_ms: Option<f64>,
    },
    Pong {
        serial: u64,
        #[serde(default)]
        buffer_ms: Option<u64>,
    },
    Stop {
        session_id: String,
        reason: StopReason,
    },
    Stopped {
        session_id: String,
    },
}

pub struct SecureControl {
    socket: TcpStream,
    noise: TransportState,
    fingerprint: String,
    code: String,
    session_id: String,
}
impl SecureControl {
    pub fn connect(address: std::net::SocketAddr, identity: &Identity) -> Result<Self> {
        let socket = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
        Self::handshake(socket, identity, true)
    }
    pub fn accept(socket: TcpStream, identity: &Identity) -> Result<Self> {
        Self::handshake(socket, identity, false)
    }
    fn handshake(mut socket: TcpStream, identity: &Identity, initiator: bool) -> Result<Self> {
        // 非阻塞监听器接入的连接先恢复阻塞读取，再使用握手超时。
        socket.set_nonblocking(false)?;
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        let builder = Builder::new(NOISE.parse()?).local_private_key(&identity.private_key);
        let mut state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        let mut scratch = vec![0; MAX_MESSAGE];
        for step in 0..3 {
            if (step % 2 == 0) == initiator {
                let len = state.write_message(&[], &mut scratch)?;
                write_frame(&mut socket, &scratch[..len])?;
            } else {
                let bytes = read_frame(&mut socket)?;
                state.read_message(&bytes, &mut scratch)?;
            }
        }
        let fingerprint = hex::encode(Sha256::digest(
            state
                .get_remote_static()
                .ok_or_else(|| anyhow::anyhow!("missing peer identity"))?,
        ));
        let hash = state.get_handshake_hash();
        let session_id = hex::encode(hash);
        let code = format!(
            "{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}",
            hash[0], hash[1], hash[2], hash[3], hash[4], hash[5]
        );
        socket.set_read_timeout(Some(Duration::from_secs(35)))?;
        Ok(Self {
            socket,
            noise: state.into_transport_mode()?,
            fingerprint,
            code,
            session_id,
        })
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn code(&self) -> &str {
        &self.code
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn peer_addr(&self) -> Result<std::net::SocketAddr> {
        Ok(self.socket.peer_addr()?)
    }
    pub fn send(&mut self, message: &Message) -> Result<()> {
        let payload = serde_json::to_vec(message)?;
        ensure!(
            payload.len() + 16 <= MAX_MESSAGE,
            "control message too large"
        );
        let mut bytes = vec![0; payload.len() + 16];
        let len = self.noise.write_message(&payload, &mut bytes)?;
        write_frame(&mut self.socket, &bytes[..len])
    }
    pub fn receive(&mut self) -> Result<Message> {
        for _ in 0..16 {
            let bytes = read_frame(&mut self.socket)?;
            let mut payload = vec![0; bytes.len()];
            let len = self.noise.read_message(&bytes, &mut payload)?;
            let message = serde_json::from_slice(&payload[..len])?;
            // 旧会话的关闭消息不能改变当前会话；限制无效消息数量以免无限等待。
            if matches!(&message, Message::Stop { session_id, .. } | Message::Stopped { session_id } if session_id != &self.session_id)
            {
                continue;
            }
            return Ok(message);
        }
        anyhow::bail!("too many stale session messages")
    }
    pub fn timeout(&self, timeout: Duration) -> Result<()> {
        Ok(self.socket.set_read_timeout(Some(timeout))?)
    }
}

fn write_frame(socket: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_MESSAGE, "oversized control frame");
    socket.write_all(&(bytes.len() as u16).to_be_bytes())?;
    socket.write_all(bytes)?;
    Ok(())
}
fn read_frame(socket: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0; 2];
    read_control_bytes(socket, &mut header)?;
    let len = u16::from_be_bytes(header) as usize;
    ensure!(
        len > 0 && len <= MAX_MESSAGE,
        "invalid control frame length"
    );
    let mut bytes = vec![0; len];
    read_control_bytes(socket, &mut bytes)?;
    Ok(bytes)
}
fn read_control_bytes(socket: &mut TcpStream, bytes: &mut [u8]) -> Result<()> {
    socket
        .read_exact(bytes)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => anyhow::anyhow!("对端已关闭控制连接"),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                anyhow::anyhow!("控制连接读取超时")
            }
            _ => anyhow::anyhow!("控制连接读取失败：{error}"),
        })
}
pub fn session_key() -> [u8; 32] {
    let mut key = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handshake_waits_for_peer_on_an_accepted_nonblocking_socket() {
        let dir = tempfile::tempdir().unwrap();
        let client_identity = Identity::load(&dir.path().join("client")).unwrap();
        let server_identity = Identity::load(&dir.path().join("server")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, accepted) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let socket = listener.accept().unwrap().0;
            socket.set_nonblocking(true).unwrap();
            ready.send(()).unwrap();
            let mut control = SecureControl::accept(socket, &server_identity).unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Hello { protocol: 2, .. }
            ));
        });
        let socket = TcpStream::connect(address).unwrap();
        accepted.recv().unwrap();
        // 让服务端先进入读取，覆盖连接已到达但首个握手包尚未到达的情况。
        std::thread::sleep(Duration::from_millis(50));
        let mut control = SecureControl::handshake(socket, &client_identity, true).unwrap();
        control
            .send(&Message::Hello {
                name: "Phone".into(),
                protocol: 2,
                control_port: None,
            })
            .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn noise_peers_agree_on_code_and_exchange_encrypted_control() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load(&dir.path().join("a")).unwrap();
        let b = Identity::load(&dir.path().join("b")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let mut peer = SecureControl::accept(listener.accept().unwrap().0, &b).unwrap();
            assert!(matches!(
                peer.receive().unwrap(),
                Message::Hello { protocol: 2, .. }
            ));
            let code = peer.code().to_owned();
            peer.send(&Message::Pending { code: code.clone() }).unwrap();
            code
        });
        let mut peer = SecureControl::connect(addr, &a).unwrap();
        peer.send(&Message::Hello {
            name: "Robin".into(),
            protocol: 2,
            control_port: Some(4212),
        })
        .unwrap();
        match peer.receive().unwrap() {
            Message::Pending { code } => assert_eq!(code, peer.code()),
            other => panic!("{other:?}"),
        }
        assert_eq!(thread.join().unwrap(), peer.code());
    }

    #[test]
    fn stale_disconnect_messages_do_not_end_the_current_session() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load(&dir.path().join("a")).unwrap();
        let b = Identity::load(&dir.path().join("b")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut control = SecureControl::accept(listener.accept().unwrap().0, &b).unwrap();
            control
                .send(&Message::Stop {
                    session_id: "old-session".into(),
                    reason: StopReason::UserDisconnect,
                })
                .unwrap();
            control
                .send(&Message::Stopped {
                    session_id: "old-session".into(),
                })
                .unwrap();
            control.send(&Message::Ready).unwrap();
            control.session_id().to_owned()
        });
        let mut control = SecureControl::connect(address, &a).unwrap();
        assert!(matches!(control.receive().unwrap(), Message::Ready));
        assert_eq!(server.join().unwrap(), control.session_id());
    }
}
