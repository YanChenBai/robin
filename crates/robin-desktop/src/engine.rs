use crate::capture;
use anyhow::{Result, bail, ensure};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use robin_core::{
    SERVICE_TYPE,
    control::{ConnectionIntent, Identity, Message, PROTOCOL_VERSION, SecureControl, StopReason},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq)]
pub struct CaptureDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}
#[derive(Clone, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub address: SocketAddr,
}
#[derive(Clone, Default, PartialEq)]
pub struct DesktopSnapshot {
    pub state: String,
    pub peer_name: String,
    pub peer_id: String,
    pub code: String,
    pub error: String,
    pub capture_name: String,
    pub capture_period_ms: f64,
    pub rtt_ms: Option<f64>,
    pub buffer_ms: u64,
    pub sent: u64,
    pub audio_level: f32,
    pub fingerprint: String,
}

pub struct DesktopEngine {
    pub peers: Arc<Mutex<BTreeMap<String, Peer>>>,
    pub state: Arc<Mutex<DesktopSnapshot>>,
    pub selected_capture: Arc<Mutex<String>>,
    discovery: Option<ServiceDaemon>,
    discovery_stop: Arc<AtomicBool>,
    discovery_worker: Option<thread::JoinHandle<()>>,
    session_stop: Arc<AtomicBool>,
    session_disconnect: Arc<AtomicBool>,
    session_worker: Option<thread::JoinHandle<()>>,
    identity: Identity,
    listener: TcpListener,
    pub addresses: Vec<String>,
    blocked: Arc<Mutex<BTreeSet<String>>>,
    policy_path: std::path::PathBuf,
}

impl DesktopEngine {
    pub fn new(identity: Identity, policy_path: std::path::PathBuf) -> Result<Self> {
        let blocked = match std::fs::read(&policy_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(error.into()),
        };
        let peers = Arc::new(Mutex::new(BTreeMap::new()));
        let state = Arc::new(Mutex::new(DesktopSnapshot {
            state: "idle".into(),
            buffer_ms: 10,
            ..Default::default()
        }));
        let listener = TcpListener::bind("0.0.0.0:4212")?;
        listener.set_nonblocking(true)?;
        let desktop_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Robin desktop".into());
        let discovery = ServiceDaemon::new()?;
        let fingerprint = identity.fingerprint();
        let service = ServiceInfo::new(
            "_robin-desktop._tcp.local.",
            identity.id(),
            &format!("robin-{}.local.", identity.id()),
            "",
            4212,
            [
                ("name", desktop_name.as_str()),
                ("protocol", "2"),
                ("fingerprint", fingerprint.as_str()),
            ]
            .as_slice(),
        )?
        .enable_addr_auto();
        discovery.register(service)?;
        let addresses = local_addresses();
        let events = discovery.browse(SERVICE_TYPE)?;
        let discovery_stop = Arc::new(AtomicBool::new(false));
        let worker_peers = peers.clone();
        let worker_stop = discovery_stop.clone();
        let discovery_worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match events.recv_timeout(Duration::from_millis(200)) {
                    Ok(ServiceEvent::ServiceResolved(info)) => {
                        if info.get_property_val_str("protocol") != Some("2") {
                            continue;
                        }
                        if let Some(address) = info.get_addresses_v4().into_iter().next() {
                            let id = info.get_fullname().to_owned();
                            let name = info
                                .get_property_val_str("name")
                                .unwrap_or(info.get_hostname())
                                .to_owned();
                            worker_peers.lock().unwrap().insert(
                                id.clone(),
                                Peer {
                                    id,
                                    name,
                                    address: SocketAddr::from((address, info.get_port())),
                                },
                            );
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, id)) => {
                        worker_peers.lock().unwrap().remove(&id);
                    }
                    _ => {}
                }
            }
        });
        Ok(Self {
            peers,
            state,
            selected_capture: Arc::new(Mutex::new(String::new())),
            discovery: Some(discovery),
            discovery_stop,
            discovery_worker: Some(discovery_worker),
            session_stop: Arc::new(AtomicBool::new(false)),
            session_disconnect: Arc::new(AtomicBool::new(false)),
            session_worker: None,
            identity,
            listener,
            addresses,
            blocked: Arc::new(Mutex::new(blocked)),
            policy_path,
        })
    }
    pub fn poll_incoming(&mut self) {
        if let Ok((socket, address)) = self.listener.accept() {
            let busy = matches!(
                self.state.lock().unwrap().state.as_str(),
                "connecting" | "pending" | "streaming" | "reconnecting"
            );
            if !busy {
                let peer = self
                    .peers
                    .lock()
                    .unwrap()
                    .values()
                    .find(|peer| peer.address.ip() == address.ip())
                    .cloned()
                    .unwrap_or_else(|| Peer {
                        id: format!("incoming-{}", address.ip()),
                        name: format!("手机 · {}", address.ip()),
                        address,
                    });
                self.launch(peer, Some(socket));
            }
        }
    }
    pub fn connect(&mut self, peer: Peer) {
        self.launch(peer, None);
    }
    fn launch(&mut self, peer: Peer, incoming: Option<TcpStream>) {
        self.cancel_session();
        // Detached old sessions retain their own snapshot and cannot overwrite a new one.
        self.state = Arc::new(Mutex::new(DesktopSnapshot {
            buffer_ms: 10,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let user_disconnect = Arc::new(AtomicBool::new(false));
        self.session_disconnect = user_disconnect.clone();
        self.session_stop = stop.clone();
        let state = self.state.clone();
        let selected = self.selected_capture.clone();
        let identity = self.identity.clone();
        let peers = self.peers.clone();
        let blocked = self.blocked.clone();
        let policy_path = self.policy_path.clone();
        self.session_worker = Some(thread::spawn(move || {
            let mut incoming = incoming;
            if !stop.load(Ordering::Acquire) {
                let current = peers
                    .lock()
                    .unwrap()
                    .get(&peer.id)
                    .cloned()
                    .unwrap_or_else(|| peer.clone());
                {
                    let mut snapshot = state.lock().unwrap();
                    snapshot.state = "connecting".into();
                    snapshot.peer_name = current.name.clone();
                    snapshot.peer_id = current.id.clone();
                    snapshot.error.clear();
                }
                let control = if let Some(socket) = incoming.take() {
                    SecureControl::accept(socket, &identity)
                } else {
                    SecureControl::connect(current.address, &identity)
                };
                let result = control.and_then(|control| {
                    let fingerprint = control.fingerprint().to_owned();
                    state.lock().unwrap().fingerprint = fingerprint.clone();
                    stream_session(
                        control,
                        &current,
                        (&stop, &user_disconnect),
                        &selected,
                        &state,
                        &blocked,
                        &policy_path,
                    )
                });
                if let Err(error) = result {
                    state.lock().unwrap().error = error.to_string();
                }
                // 手机统一负责自动重连，电脑不再同时发起竞争连接。
            }
            let mut snapshot = state.lock().unwrap();
            snapshot.state = "idle".into();
            snapshot.code.clear();
            snapshot.rtt_ms = None;
        }));
    }
    pub fn disconnect(&mut self) {
        self.session_disconnect.store(true, Ordering::Release);
        let fingerprint = self.state.lock().unwrap().fingerprint.clone();
        if !fingerprint.is_empty() {
            self.blocked.lock().unwrap().insert(fingerprint);
            if let Err(error) = save_policy(&self.blocked, &self.policy_path) {
                self.state.lock().unwrap().error = error.to_string();
            }
        }
        self.cancel_session();
    }
    fn cancel_session(&mut self) {
        self.session_stop.store(true, Ordering::Release);
        let mut snapshot = self.state.lock().unwrap().clone();
        snapshot.state = "idle".into();
        snapshot.code.clear();
        snapshot.rtt_ms = None;
        self.state = Arc::new(Mutex::new(snapshot));
        // Finished tasks are joined; a pending handshake may finish after the view command.
        if let Some(worker) = self.session_worker.take() {
            thread::spawn(move || {
                let _ = worker.join();
            });
        }
    }
}
impl Drop for DesktopEngine {
    fn drop(&mut self) {
        self.cancel_session();
        self.discovery_stop.store(true, Ordering::Release);
        if let Some(discovery) = self.discovery.take() {
            let _ = discovery.shutdown();
        }
        if let Some(worker) = self.discovery_worker.take() {
            let _ = worker.join();
        }
    }
}

fn stream_session(
    mut control: SecureControl,
    peer: &Peer,
    cancellation: (&Arc<AtomicBool>, &Arc<AtomicBool>),
    selected: &Arc<Mutex<String>>,
    state: &Arc<Mutex<DesktopSnapshot>>,
    blocked: &Arc<Mutex<BTreeSet<String>>>,
    policy_path: &std::path::Path,
) -> Result<()> {
    let (stop, user_disconnect) = cancellation;
    let desktop_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Robin desktop".into());
    control.send(&Message::Hello {
        name: desktop_name,
        protocol: PROTOCOL_VERSION,
        control_port: Some(4212),
    })?;
    if !authorize_request(&mut control, blocked, policy_path, cancellation)? {
        return Ok(());
    }
    match control.receive()? {
        Message::Pending { code } => {
            ensure!(code == control.code(), "connection code mismatch");
            let mut snapshot = state.lock().unwrap();
            snapshot.state = "pending".into();
            snapshot.code = code;
        }
        _ => bail!("unexpected connection response"),
    }
    let (key, port, buffer_ms) = match control.receive()? {
        Message::Accept {
            key,
            audio_port,
            buffer_ms,
        } => (key, audio_port, buffer_ms),
        Message::Reject { reason } => {
            state.lock().unwrap().state = "rejected".into();
            bail!(reason);
        }
        _ => bail!("unexpected approval response"),
    };
    ensure!(!stop.load(Ordering::Acquire), "connection cancelled");
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(SocketAddr::new(peer.address.ip(), port))?;
    udp.set_write_timeout(Some(Duration::from_millis(10)))?;
    {
        let mut snapshot = state.lock().unwrap();
        snapshot.state = "streaming".into();
        snapshot.buffer_ms = buffer_ms;
        snapshot.sent = 0;
    }
    let capture_stop = Arc::new(AtomicBool::new(false));
    let capture_flag = capture_stop.clone();
    let capture_state = state.clone();
    let capture_selected = selected.clone();
    let failed = Arc::new(AtomicBool::new(false));
    let capture_failed = failed.clone();
    let capture = thread::spawn(move || {
        if let Err(error) = capture::run(
            udp,
            key,
            capture_flag,
            capture_selected,
            capture_state.clone(),
        ) {
            capture_state.lock().unwrap().error = error.to_string();
            capture_failed.store(true, Ordering::Release);
        }
    });
    control.timeout(Duration::from_secs(2))?;
    let session_id = control.session_id().to_owned();
    let mut serial = 0;
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) && !failed.load(Ordering::Acquire) {
            let now = Instant::now();
            let snapshot = state.lock().unwrap().clone();
            control.send(&Message::Ping {
                serial,
                rtt_ms: snapshot.rtt_ms,
                capture_period_ms: Some(snapshot.capture_period_ms),
            })?;
            match control.receive()? {
                Message::Pong {
                    serial: response,
                    buffer_ms,
                } if response == serial => {
                    if let Some(buffer_ms) = buffer_ms {
                        state.lock().unwrap().buffer_ms = buffer_ms;
                    }
                }
                Message::Stop {
                    session_id: response,
                    reason,
                } if response == session_id => {
                    if reason == StopReason::UserDisconnect {
                        blocked
                            .lock()
                            .unwrap()
                            .insert(control.fingerprint().to_owned());
                        save_policy(blocked, policy_path)?;
                    }
                    capture_stop.store(true, Ordering::Release);
                    control.send(&Message::Stopped {
                        session_id: response,
                    })?;
                    return Ok(());
                }
                _ => bail!("invalid heartbeat"),
            }
            state.lock().unwrap().rtt_ms = Some(now.elapsed().as_secs_f64() * 1000.0);
            serial += 1;
            for _ in 0..5 {
                if stop.load(Ordering::Acquire) || failed.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if stop.load(Ordering::Acquire) {
            capture_stop.store(true, Ordering::Release);
            control.send(&Message::Stop {
                session_id: session_id.clone(),
                reason: if user_disconnect.load(Ordering::Acquire) {
                    StopReason::UserDisconnect
                } else {
                    StopReason::ReceiverClosed
                },
            })?;
            for _ in 0..2 {
                match control.receive()? {
                    Message::Stopped {
                        session_id: response,
                    } if response == session_id => break,
                    Message::Pong { .. } => continue,
                    Message::Stop {
                        session_id: response,
                        ..
                    } if response == session_id => {
                        control.send(&Message::Stopped {
                            session_id: response,
                        })?;
                        break;
                    }
                    _ => bail!("invalid disconnect acknowledgement"),
                }
            }
        }
        Ok(())
    })();
    capture_stop.store(true, Ordering::Release);
    let _ = capture.join();
    result
}

fn authorize_request(
    control: &mut SecureControl,
    blocked: &Mutex<BTreeSet<String>>,
    path: &std::path::Path,
    cancellation: (&Arc<AtomicBool>, &Arc<AtomicBool>),
) -> Result<bool> {
    let Message::Request { intent } = control.receive()? else {
        bail!("unsupported connection intent");
    };
    let mut policy = blocked.lock().unwrap();
    let cancelled =
        cancellation.0.load(Ordering::Acquire) || cancellation.1.load(Ordering::Acquire);
    let rejected =
        cancelled || (intent == ConnectionIntent::Resume && policy.contains(control.fingerprint()));
    if !rejected && intent == ConnectionIntent::Manual {
        policy.remove(control.fingerprint());
    }
    drop(policy);
    if rejected {
        let session_id = control.session_id().to_owned();
        control.timeout(Duration::from_secs(2))?;
        control.send(&Message::Stop {
            session_id: session_id.clone(),
            reason: StopReason::UserDisconnect,
        })?;
        ensure!(
            matches!(control.receive()?, Message::Stopped { session_id: response } if response == session_id),
            "invalid disconnect acknowledgement"
        );
        return Ok(false);
    }
    save_policy(blocked, path)?;
    control.send(&Message::Ready)?;
    Ok(true)
}

fn save_policy(blocked: &Mutex<BTreeSet<String>>, path: &std::path::Path) -> Result<()> {
    // 同一把锁覆盖临时文件写入与替换，避免 UI 与会话线程互相覆盖。
    let policy = blocked.lock().unwrap();
    let bytes = serde_json::to_vec(&*policy)?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    drop(policy);
    Ok(())
}

pub(crate) fn local_addresses() -> Vec<String> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(ip) if !ip.is_link_local() => Some(format!("{ip}:4212")),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_disconnect_blocks_resume_but_manual_connection_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Identity::load(&dir.path().join("desktop.key")).unwrap();
        let phone = Identity::load(&dir.path().join("phone.key")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let path = dir.path().join("disconnected.json");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            for intent in [ConnectionIntent::Resume, ConnectionIntent::Manual] {
                let mut control =
                    SecureControl::accept(listener.accept().unwrap().0, &desktop).unwrap();
                let fingerprint = control.fingerprint().to_owned();
                let blocked = if intent == ConnectionIntent::Resume {
                    let policy = Mutex::new(BTreeSet::from([fingerprint.clone()]));
                    save_policy(&policy, &server_path).unwrap();
                    Mutex::new(
                        serde_json::from_slice(&std::fs::read(&server_path).unwrap()).unwrap(),
                    )
                } else {
                    Mutex::new(
                        serde_json::from_slice(&std::fs::read(&server_path).unwrap()).unwrap(),
                    )
                };
                let stop = Arc::new(AtomicBool::new(false));
                let manual = Arc::new(AtomicBool::new(false));
                let allowed =
                    authorize_request(&mut control, &blocked, &server_path, (&stop, &manual))
                        .unwrap();
                assert_eq!(allowed, intent == ConnectionIntent::Manual);
                assert_eq!(blocked.lock().unwrap().contains(&fingerprint), !allowed);
            }
        });
        for intent in [ConnectionIntent::Resume, ConnectionIntent::Manual] {
            let mut control = SecureControl::connect(address, &phone).unwrap();
            control.send(&Message::Request { intent }).unwrap();
            if intent == ConnectionIntent::Resume {
                let session_id = control.session_id().to_owned();
                assert!(
                    matches!(control.receive().unwrap(), Message::Stop { session_id: response, reason: StopReason::UserDisconnect } if response == session_id)
                );
                control.send(&Message::Stopped { session_id }).unwrap();
            } else {
                assert!(matches!(control.receive().unwrap(), Message::Ready));
            }
        }
        server.join().unwrap();
        let policy: BTreeSet<String> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(policy.is_empty());
    }
}
