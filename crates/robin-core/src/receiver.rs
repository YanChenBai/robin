use crate::{
    SAMPLE_RATE, StereoFrame,
    audio::{AudioCipher, MAX_DATAGRAM, PlayoutBuffer},
    control::{ConnectionIntent, Identity, Message, SecureControl, StopReason, session_key},
};
use anyhow::{Result, bail, ensure};
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReceiverSnapshot {
    pub state: String,
    pub peer_name: String,
    pub fingerprint: String,
    pub code: String,
    pub error: String,
    pub port: u16,
    pub buffer_ms: u64,
    pub buffer_floor_ms: u64,
    pub queued_ms: f64,
    pub jitter_queued_ms: f64,
    pub network_rtt_ms: Option<f64>,
    pub capture_period_ms: Option<f64>,
    pub estimated_latency_ms: Option<f64>,
    pub received: u64,
    pub lost: u64,
    pub late: u64,
    pub underruns: u64,
    pub output_frames: u64,
    pub history: Vec<ConnectionRecord>,
    pub auto_buffer: bool,
    pub background: bool,
    pub paused: bool,
    pub waveform: Vec<f32>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ConnectionRecord {
    pub fingerprint: String,
    pub name: String,
    pub auto_reconnect: bool,
    #[serde(default)]
    pub address: String,
}

#[derive(Default)]
pub struct PlaybackStats {
    active: AtomicBool,
    started: AtomicBool,
    target_frames: AtomicUsize,
    paused: AtomicBool,
    waveform: [AtomicU64; 32],
    pub underruns: AtomicU64,
    pub queued_frames: AtomicU64,
    pub output_frames: AtomicU64,
    pub failed: AtomicBool,
}

/// Callback-owned lock-free reader. No network, allocations or mutexes on this path.
pub struct PlaybackReader {
    consumer: Consumer<StereoFrame>,
    stats: Arc<PlaybackStats>,
    was_active: bool,
    primed: bool,
    last_frame: StereoFrame,
    fade_from: StereoFrame,
    fade_remaining: usize,
    fade_out_remaining: usize,
}
impl PlaybackReader {
    pub fn fill(&mut self, output: &mut [StereoFrame]) {
        if !self.stats.active.load(Ordering::Acquire) || self.stats.paused.load(Ordering::Acquire) {
            while self.consumer.pop().is_ok() {}
            output.fill((0, 0));
            self.was_active = false;
            self.primed = false;
            self.last_frame = (0, 0);
            self.fade_out_remaining = 0;
            for level in &self.stats.waveform {
                level.store(0, Ordering::Relaxed);
            }
            self.stats.started.store(false, Ordering::Relaxed);
            self.stats.queued_frames.store(0, Ordering::Relaxed);
            return;
        }
        let target_frames = self.stats.target_frames.load(Ordering::Relaxed);
        let available = self.consumer.slots();
        // Bounded drift/overload correction: discard stale queue instead of growing latency.
        let maximum = target_frames + SAMPLE_RATE as usize / 100 + output.len() * 2;
        for _ in maximum..available {
            let _ = self.consumer.pop();
        }
        if available > maximum {
            self.fade_from = self.last_frame;
            self.fade_remaining = 48;
        }
        if !self.primed {
            if self.consumer.slots() < target_frames + output.len() {
                for frame in output {
                    *frame = self.fade_to_silence();
                }
                self.stats
                    .queued_frames
                    .store(self.consumer.slots() as u64, Ordering::Relaxed);
                return;
            }
            self.primed = true;
            self.fade_from = (0, 0);
            self.fade_remaining = 48;
            self.stats.started.store(true, Ordering::Relaxed);
        }
        let mut missing = false;
        let mut peaks = [0_u64; 32];
        let frames_per_band = output.len().div_ceil(32).max(1);
        for (ix, frame) in output.iter_mut().enumerate() {
            match self.consumer.pop() {
                Ok(value) => {
                    *frame = if self.fade_remaining > 0 {
                        let weight = self.fade_remaining as i32;
                        self.fade_remaining -= 1;
                        (
                            ((self.fade_from.0 as i32 * weight + value.0 as i32 * (48 - weight))
                                / 48) as i16,
                            ((self.fade_from.1 as i32 * weight + value.1 as i32 * (48 - weight))
                                / 48) as i16,
                        )
                    } else {
                        value
                    };
                    self.was_active = true;
                }
                Err(_) => {
                    if !missing {
                        self.fade_out_remaining = 48;
                    }
                    *frame = self.fade_to_silence();
                    missing |= self.was_active;
                }
            }
            self.last_frame = *frame;
            let peak = (frame.0 as i32)
                .unsigned_abs()
                .max((frame.1 as i32).unsigned_abs());
            let band = (ix / frames_per_band).min(31);
            peaks[band] = peaks[band].max(peak as u64);
        }
        for (level, peak) in self.stats.waveform.iter().zip(peaks) {
            level.store(peak, Ordering::Relaxed);
        }
        if missing {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            self.primed = false;
            self.stats.started.store(false, Ordering::Relaxed);
        }
        self.stats
            .queued_frames
            .store(self.consumer.slots() as u64, Ordering::Relaxed);
    }
    pub fn stats(&self) -> &Arc<PlaybackStats> {
        &self.stats
    }
    fn fade_to_silence(&mut self) -> StereoFrame {
        self.fade_out_remaining = self.fade_out_remaining.saturating_sub(1);
        self.last_frame = (
            (self.last_frame.0 as i32 * self.fade_out_remaining as i32 / 48) as i16,
            (self.last_frame.1 as i32 * self.fade_out_remaining as i32 / 48) as i16,
        );
        self.last_frame
    }
}

enum Decision {
    Allow(bool),
    Reject,
}

pub struct Receiver {
    state: Arc<Mutex<ReceiverSnapshot>>,
    stats: Arc<PlaybackStats>,
    pending: Arc<Mutex<Option<mpsc::Sender<Decision>>>>,
    stop: Arc<AtomicBool>,
    disconnect: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    connect: mpsc::Sender<(SocketAddr, Option<String>, ConnectionIntent)>,
    discovered: Arc<Mutex<BTreeMap<String, (String, SocketAddr)>>>,
}

impl Receiver {
    pub fn start(
        identity: Identity,
        port: u16,
        buffer_ms: u64,
        history: Vec<ConnectionRecord>,
    ) -> Result<(Self, PlaybackReader)> {
        PlayoutBuffer::new(buffer_ms)?;
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(Mutex::new(ReceiverSnapshot {
            state: "waiting".into(),
            port,
            buffer_ms,
            buffer_floor_ms: buffer_ms,
            history,
            auto_buffer: false,
            ..Default::default()
        }));
        let stats = Arc::new(PlaybackStats {
            target_frames: AtomicUsize::new((SAMPLE_RATE as u64 * buffer_ms / 1000) as usize),
            ..Default::default()
        });
        let pending = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let disconnect = Arc::new(AtomicBool::new(false));
        let (mut producer, consumer) = RingBuffer::new(SAMPLE_RATE as usize / 5);
        let (connect, connections) =
            mpsc::channel::<(SocketAddr, Option<String>, ConnectionIntent)>();
        let worker_state = state.clone();
        let worker_pending = pending.clone();
        let worker_stop = stop.clone();
        let worker_disconnect = disconnect.clone();
        let worker_stats = stats.clone();
        let discovered = Arc::new(Mutex::new(BTreeMap::<String, (String, SocketAddr)>::new()));
        let worker_discovered = discovered.clone();
        let worker = thread::Builder::new()
            .name("robin-receiver".into())
            .spawn(move || {
                let mut retry_at = Instant::now();
                let mut retry_ix = 0;
                let mut retry = ReconnectBackoff::default();
                while !worker_stop.load(Ordering::Acquire) {
                    // 优先接收已经到达的连接，避免双方同时握手时互相等待。
                    let incoming = match listener.accept() {
                        Ok((socket, _)) => Some(Ok(socket)),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
                        Err(error) => Some(Err(anyhow::Error::from(error))),
                    };
                    let request = if incoming.is_some() {
                        None
                    } else {
                        connections.try_recv().ok().or_else(|| {
                            if Instant::now() < retry_at {
                                return None;
                            }
                            retry_at = Instant::now() + Duration::from_secs(3);
                            let snapshot = worker_state.lock().unwrap();
                            let records = snapshot
                                .history
                                .iter()
                                .filter(|r| r.auto_reconnect)
                                .collect::<Vec<_>>();
                            if records.is_empty() {
                                return None;
                            }
                            let record = records[retry_ix % records.len()];
                            let attempt = retry_ix / records.len();
                            retry_ix += 1;
                            reconnect_address(record, &worker_discovered.lock().unwrap(), attempt)
                                .map(|address| {
                                    (
                                        address,
                                        Some(record.fingerprint.clone()),
                                        ConnectionIntent::Resume,
                                    )
                                })
                        })
                    };
                    let intent = request
                        .as_ref()
                        .map(|(_, _, intent)| *intent)
                        .unwrap_or(ConnectionIntent::Manual);
                    let connection = if let Some(socket) = incoming {
                        worker_disconnect.store(false, Ordering::Release);
                        Some(socket.and_then(|socket| SecureControl::accept(socket, &identity)))
                    } else if let Some((address, expected, _)) = request {
                        worker_disconnect.store(false, Ordering::Release);
                        {
                            let mut snapshot = worker_state.lock().unwrap();
                            snapshot.state = "connecting".into();
                            snapshot.error.clear();
                            if let Some(record) = snapshot
                                .history
                                .iter()
                                .find(|record| Some(&record.fingerprint) == expected.as_ref())
                                .cloned()
                            {
                                snapshot.peer_name = record.name;
                                snapshot.fingerprint = record.fingerprint;
                            }
                        }
                        Some(
                            SecureControl::connect(address, &identity).and_then(|control| {
                                if let Some(expected) = expected {
                                    ensure!(
                                        control.fingerprint() == expected,
                                        "电脑身份已改变，请重新确认连接"
                                    );
                                }
                                Ok(control)
                            }),
                        )
                    } else {
                        None
                    };
                    let Some(connection) = connection else {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    };
                    let result = connection.and_then(|control| {
                        receive_session(
                            control,
                            &worker_state,
                            &worker_pending,
                            (&worker_stop, &worker_disconnect),
                            &mut producer,
                            &worker_stats,
                            intent,
                        )
                    });
                    worker_stats.active.store(false, Ordering::Release);
                    worker_stats.started.store(false, Ordering::Relaxed);
                    *worker_pending.lock().unwrap() = None;
                    let mut snapshot = worker_state.lock().unwrap();
                    snapshot.state = "waiting".into();
                    snapshot.code.clear();
                    snapshot.peer_name.clear();
                    snapshot.fingerprint.clear();
                    snapshot.network_rtt_ms = None;
                    snapshot.capture_period_ms = None;
                    snapshot.estimated_latency_ms = None;
                    snapshot.jitter_queued_ms = 0.0;
                    snapshot.buffer_ms = if snapshot.background {
                        snapshot.buffer_floor_ms.max(50)
                    } else {
                        snapshot.buffer_floor_ms
                    };
                    worker_stats.target_frames.store(
                        (SAMPLE_RATE as u64 * snapshot.buffer_ms / 1000) as usize,
                        Ordering::Release,
                    );
                    let retry_delay = retry.after_attempt(result.is_ok());
                    if let Err(error) = result {
                        snapshot.error = error.to_string();
                    }
                    retry_at = Instant::now() + retry_delay;
                }
            })?;
        Ok((
            Self {
                state,
                stats: stats.clone(),
                pending,
                stop,
                disconnect,
                worker: Some(worker),
                connect,
                discovered,
            },
            PlaybackReader {
                consumer,
                stats,
                was_active: false,
                primed: false,
                last_frame: (0, 0),
                fade_from: (0, 0),
                fade_remaining: 0,
                fade_out_remaining: 0,
            },
        ))
    }
    pub fn snapshot(&self) -> ReceiverSnapshot {
        let mut snapshot = self.state.lock().unwrap().clone();
        snapshot.paused = self.stats.paused.load(Ordering::Acquire);
        snapshot.waveform = self
            .stats
            .waveform
            .iter()
            .map(|v| v.load(Ordering::Relaxed) as f32 / 32768.0)
            .collect();
        snapshot.underruns = self.stats.underruns.load(Ordering::Relaxed);
        snapshot.queued_ms =
            self.stats.queued_frames.load(Ordering::Relaxed) as f64 / SAMPLE_RATE as f64 * 1000.0;
        snapshot.output_frames = self.stats.output_frames.load(Ordering::Relaxed);
        if snapshot.state == "playing" && !self.stats.started.load(Ordering::Relaxed) {
            snapshot.state = "buffering".into();
        }
        if snapshot.paused && matches!(snapshot.state.as_str(), "playing" | "buffering") {
            snapshot.state = "paused".into();
        }
        snapshot.estimated_latency_ms = snapshot
            .network_rtt_ms
            .zip(snapshot.capture_period_ms)
            .map(|(rtt, capture)| {
                rtt / 2.0
                    + capture
                    + snapshot.queued_ms
                    + snapshot.output_frames as f64 / SAMPLE_RATE as f64 * 1000.0
            });
        if self.stats.failed.load(Ordering::Acquire) {
            snapshot.state = "error".into();
            snapshot.error = "音频输出设备已断开，请关闭接收后重新开启".into();
        }
        snapshot
    }
    pub fn stats_output_frames(&self, frames: u64) {
        self.stats.output_frames.store(frames, Ordering::Relaxed);
    }
    pub fn history(&self) -> Vec<ConnectionRecord> {
        self.state.lock().unwrap().history.clone()
    }
    pub fn report_error(&self, error: String) {
        self.state.lock().unwrap().error = error;
    }
    pub fn approve(&self, remember: bool) -> Result<()> {
        self.decide(Decision::Allow(remember))
    }
    pub fn reject(&self) -> Result<()> {
        self.decide(Decision::Reject)
    }
    fn decide(&self, decision: Decision) -> Result<()> {
        let sender = self
            .pending
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("no pending connection"))?;
        sender
            .send(decision)
            .map_err(|_| anyhow::anyhow!("connection request expired"))
    }
    pub fn disconnect(&self) {
        // 主动断开停用自动连接，保留已经允许过的电脑记录。
        let mut snapshot = self.state.lock().unwrap();
        let fingerprint = snapshot.fingerprint.clone();
        for record in &mut snapshot.history {
            if record.fingerprint == fingerprint {
                record.auto_reconnect = false;
            }
        }
        self.disconnect.store(true, Ordering::Release);
        if let Some(sender) = self.pending.lock().unwrap().take() {
            let _ = sender.send(Decision::Reject);
        }
    }
    pub fn set_auto_reconnect(&self, fingerprint: &str, enabled: bool) {
        for record in &mut self.state.lock().unwrap().history {
            if record.fingerprint == fingerprint {
                record.auto_reconnect = enabled;
            }
        }
    }
    /// 发现结果只提供候选地址，实际连接仍须通过已保存指纹验证。
    pub fn update_discovered_desktop(
        &self,
        service: String,
        fingerprint: String,
        address: Option<SocketAddr>,
    ) {
        let mut discovered = self.discovered.lock().unwrap();
        discovered.remove(&service);
        if let Some(address) = address
            && address.is_ipv4()
            && address.port() != 0
            && !address.ip().is_unspecified()
            && fingerprint.len() == 64
            && fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
        {
            discovered.insert(service, (fingerprint, address));
        }
    }
    pub fn connect(&self, address: SocketAddr, fingerprint: Option<String>) -> Result<()> {
        let mut snapshot = self.state.lock().unwrap();
        ensure!(snapshot.state == "waiting", "请先断开当前电脑");
        ensure!(
            address.port() != 0 && !address.ip().is_unspecified(),
            "请输入有效的电脑 IP 和端口"
        );
        self.connect
            .send((address, fingerprint, ConnectionIntent::Manual))
            .map_err(|_| anyhow::anyhow!("接收已关闭"))?;
        snapshot.state = "connecting".into();
        snapshot.error.clear();
        Ok(())
    }
    pub fn set_buffer(&self, buffer_ms: u64, automatic: bool) -> Result<()> {
        PlayoutBuffer::new(buffer_ms)?;
        let mut state = self.state.lock().unwrap();
        state.buffer_floor_ms = buffer_ms;
        state.buffer_ms = if state.background {
            buffer_ms.max(50)
        } else {
            buffer_ms
        };
        state.auto_buffer = automatic;
        self.stats.target_frames.store(
            (SAMPLE_RATE as u64 * state.buffer_ms / 1000) as usize,
            Ordering::Release,
        );
        Ok(())
    }
    pub fn set_background(&self, background: bool) {
        let mut state = self.state.lock().unwrap();
        state.background = background;
        // 后台网络调度存在额外抖动；前台恢复用户设置，不改写保存的下限。
        state.buffer_ms = if background {
            state.buffer_floor_ms.max(50)
        } else {
            state.buffer_floor_ms
        };
        self.stats.target_frames.store(
            (SAMPLE_RATE as u64 * state.buffer_ms / 1000) as usize,
            Ordering::Release,
        );
    }
    pub fn set_paused(&self, paused: bool) {
        self.stats.paused.store(paused, Ordering::Release);
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.disconnect.store(true, Ordering::Release);
        if let Some(sender) = self.pending.lock().unwrap().take() {
            let _ = sender.send(Decision::Reject);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct ReconnectBackoff {
    failures: u32,
}

fn reconnect_address(
    record: &ConnectionRecord,
    discovered: &BTreeMap<String, (String, SocketAddr)>,
    attempt: usize,
) -> Option<SocketAddr> {
    let mut addresses = discovered
        .values()
        .filter(|(fingerprint, _)| fingerprint == &record.fingerprint)
        .map(|(_, address)| *address)
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    if let Ok(saved) = record.address.parse()
        && !addresses.contains(&saved)
    {
        addresses.push(saved);
    }
    if addresses.is_empty() {
        return None;
    }
    Some(addresses[attempt % addresses.len()])
}
impl ReconnectBackoff {
    fn after_attempt(&mut self, succeeded: bool) -> Duration {
        if succeeded {
            self.failures = 0;
        }
        let seconds = (3 * (1u64 << self.failures.min(4))).min(30);
        if !succeeded {
            self.failures = self.failures.saturating_add(1);
        }
        Duration::from_secs(seconds)
    }
}

fn receive_session(
    mut control: SecureControl,
    state: &Arc<Mutex<ReceiverSnapshot>>,
    pending: &Arc<Mutex<Option<mpsc::Sender<Decision>>>>,
    cancellation: (&Arc<AtomicBool>, &Arc<AtomicBool>),
    producer: &mut Producer<StereoFrame>,
    stats: &Arc<PlaybackStats>,
    intent: ConnectionIntent,
) -> Result<()> {
    let (stop, disconnect) = cancellation;
    let (name, control_port) = match control.receive()? {
        Message::Hello {
            name,
            protocol: 2,
            control_port,
        } if name.chars().count() <= 80 => (name, control_port),
        _ => bail!("unsupported connection request"),
    };
    let fingerprint = control.fingerprint().to_owned();
    control.send(&Message::Request { intent })?;
    match control.receive()? {
        Message::Ready => {}
        Message::Stop {
            session_id,
            reason: StopReason::UserDisconnect,
        } if session_id == control.session_id() => {
            for record in &mut state.lock().unwrap().history {
                if record.fingerprint == fingerprint {
                    record.auto_reconnect = false;
                }
            }
            control.send(&Message::Stopped { session_id })?;
            return Ok(());
        }
        _ => bail!("电脑未允许连接"),
    }
    // 记录仅在允许连接后保存，授权与自动连接偏好分别处理。
    let remembered = state
        .lock()
        .unwrap()
        .history
        .iter()
        .find(|r| r.fingerprint == fingerprint)
        .cloned();
    let (tx, rx) = mpsc::channel();
    *pending.lock().unwrap() = Some(tx);
    {
        let mut snapshot = state.lock().unwrap();
        snapshot.state = if remembered.is_some() {
            "connecting".into()
        } else {
            "pending".into()
        };
        snapshot.peer_name = name.clone();
        snapshot.fingerprint = fingerprint.clone();
        snapshot.code = control.code().to_owned();
        snapshot.error.clear();
    }
    control.send(&Message::Pending {
        code: control.code().to_owned(),
    })?;
    let decision = if let Some(record) = remembered {
        Decision::Allow(record.auto_reconnect)
    } else {
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or(Decision::Reject)
    };
    *pending.lock().unwrap() = None;
    ensure!(
        !stop.load(Ordering::Acquire) && !disconnect.load(Ordering::Acquire),
        "connection cancelled"
    );
    let remember = match decision {
        Decision::Allow(remember) => remember,
        Decision::Reject => {
            control.send(&Message::Reject {
                reason: "手机未允许连接".into(),
            })?;
            return Ok(());
        }
    };
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.set_read_timeout(Some(Duration::from_millis(1)))?;
    let key = session_key();
    let buffer_ms = state.lock().unwrap().buffer_ms;
    control.send(&Message::Accept {
        key,
        audio_port: udp.local_addr()?.port(),
        buffer_ms,
    })?;
    {
        let mut snapshot = state.lock().unwrap();
        snapshot.history.retain(|r| r.fingerprint != fingerprint);
        snapshot.history.insert(
            0,
            ConnectionRecord {
                fingerprint,
                name,
                auto_reconnect: remember,
                address: control_port
                    .filter(|port| *port > 0)
                    .map(|port| {
                        SocketAddr::new(control.peer_addr().unwrap().ip(), port).to_string()
                    })
                    .unwrap_or_default(),
            },
        );
        snapshot.history.truncate(50);
        snapshot.state = "buffering".into();
        snapshot.received = 0;
        snapshot.lost = 0;
        snapshot.late = 0;
    }
    stats.underruns.store(0, Ordering::Relaxed);
    stats.active.store(true, Ordering::Release);
    let peer_ip = control.peer_addr()?.ip();
    control.timeout(Duration::from_secs(3))?;
    let session_stop = Arc::new(AtomicBool::new(false));
    let control_stop = session_stop.clone();
    let global_stop = stop.clone();
    let manual_stop = disconnect.clone();
    let control_state = state.clone();
    let heartbeat = thread::spawn(move || {
        let session_id = control.session_id().to_owned();
        loop {
            if global_stop.load(Ordering::Acquire) || manual_stop.load(Ordering::Acquire) {
                let reason = if global_stop.load(Ordering::Acquire) {
                    StopReason::ReceiverClosed
                } else {
                    StopReason::UserDisconnect
                };
                let _ = control.send(&Message::Stop {
                    session_id: session_id.clone(),
                    reason,
                });
                // 允许在途心跳先到达；断开确认只接受当前握手的会话标识。
                for _ in 0..2 {
                    match control.receive() {
                        Ok(Message::Stopped {
                            session_id: response,
                        }) if response == session_id => break,
                        Ok(Message::Ping { serial, .. }) => {
                            let _ = control.send(&Message::Pong {
                                serial,
                                buffer_ms: None,
                            });
                        }
                        Ok(Message::Stop {
                            session_id: response,
                            ..
                        }) if response == session_id => {
                            let _ = control.send(&Message::Stopped {
                                session_id: response,
                            });
                            break;
                        }
                        _ => break,
                    }
                }
                break;
            }
            if control_stop.load(Ordering::Acquire) {
                break;
            }
            match control.receive() {
                Ok(Message::Ping {
                    serial,
                    rtt_ms,
                    capture_period_ms,
                }) => {
                    {
                        let mut snapshot = control_state.lock().unwrap();
                        snapshot.network_rtt_ms = rtt_ms.filter(|v| v.is_finite() && *v >= 0.0);
                        snapshot.capture_period_ms =
                            capture_period_ms.filter(|v| v.is_finite() && *v >= 0.0);
                    }
                    let buffer_ms = Some(control_state.lock().unwrap().buffer_ms);
                    if control.send(&Message::Pong { serial, buffer_ms }).is_err() {
                        break;
                    }
                }
                Ok(Message::Stop {
                    session_id: response,
                    reason,
                }) if response == session_id => {
                    if reason == StopReason::UserDisconnect {
                        let mut snapshot = control_state.lock().unwrap();
                        let fingerprint = snapshot.fingerprint.clone();
                        for record in &mut snapshot.history {
                            if record.fingerprint == fingerprint {
                                record.auto_reconnect = false;
                            }
                        }
                    }
                    let _ = control.send(&Message::Stopped {
                        session_id: response,
                    });
                    break;
                }
                _ => break,
            }
        }
        control_stop.store(true, Ordering::Release);
    });
    let mut playout = PlayoutBuffer::new(buffer_ms)?;
    let cipher = AudioCipher::new(&key);
    let mut bytes = [0; MAX_DATAGRAM + 1];
    let mut last_audio = Instant::now();
    let mut received = 0;
    let mut adaptation = BufferAdaptation::new(buffer_ms, Instant::now());
    while !stop.load(Ordering::Acquire)
        && !disconnect.load(Ordering::Acquire)
        && !session_stop.load(Ordering::Acquire)
    {
        match udp.recv_from(&mut bytes) {
            Ok((len, source)) if source.ip() == peer_ip => {
                if let Ok((sequence, frames)) = cipher.open(&bytes[..len]) {
                    received += 1;
                    last_audio = Instant::now();
                    playout.push(sequence, frames, last_audio);
                }
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        if last_audio.elapsed() > Duration::from_secs(3) {
            break;
        }
        let now = Instant::now();
        {
            let mut snapshot = state.lock().unwrap();
            adaptation.floor = if snapshot.background {
                snapshot.buffer_floor_ms.max(50)
            } else {
                snapshot.buffer_floor_ms
            };
            let adjusted = adaptation.update(
                snapshot.buffer_ms,
                snapshot.auto_buffer || snapshot.background,
                stats.underruns.load(Ordering::Relaxed),
                playout.late(),
                now,
            );
            snapshot.buffer_ms = adjusted;
            stats.target_frames.store(
                (SAMPLE_RATE as u64 * adjusted / 1000) as usize,
                Ordering::Release,
            );
            playout.set_delay(adjusted)?;
        }
        for _ in 0..64 {
            let Some(frames) = playout.pop_due(now) else {
                break;
            };
            for frame in frames {
                let _ = producer.push(frame);
            }
        }
        let mut snapshot = state.lock().unwrap();
        if received > 0 {
            snapshot.state = "playing".into();
        }
        snapshot.received = received;
        snapshot.lost = playout.lost();
        snapshot.late = playout.late();
        snapshot.jitter_queued_ms = playout.depth_ms();
    }
    session_stop.store(true, Ordering::Release);
    let _ = heartbeat.join();
    Ok(())
}

// 欠载或迟到时增加缓冲，稳定十秒后缓慢回落；手动模式始终保持用户值。
struct BufferAdaptation {
    floor: u64,
    current: u64,
    underruns: u64,
    late: u64,
    stable_since: Instant,
}
impl BufferAdaptation {
    fn new(floor: u64, now: Instant) -> Self {
        Self {
            floor,
            current: floor,
            underruns: 0,
            late: 0,
            stable_since: now,
        }
    }
    fn update(
        &mut self,
        requested: u64,
        automatic: bool,
        underruns: u64,
        late: u64,
        now: Instant,
    ) -> u64 {
        if requested != self.current {
            self.current = requested;
            self.stable_since = now;
        }
        if automatic {
            self.current = self.current.max(self.floor);
            if underruns > self.underruns || late > self.late {
                self.current = (self.current + 5).min(100);
                self.stable_since = now;
            } else if now.duration_since(self.stable_since) >= Duration::from_secs(10) {
                self.current = self.current.saturating_sub(1).max(self.floor);
                self.stable_since = now;
            }
        }
        self.underruns = underruns;
        self.late = late;
        self.current
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;

    #[test]
    fn discovery_prefers_trusted_identity_and_keeps_saved_address_as_fallback() {
        let record = ConnectionRecord {
            fingerprint: "a".repeat(64),
            name: "Desktop".into(),
            auto_reconnect: true,
            address: "192.168.1.2:4212".into(),
        };
        let discovered = BTreeMap::from([
            (
                "trusted".into(),
                (
                    record.fingerprint.clone(),
                    "192.168.1.3:4212".parse().unwrap(),
                ),
            ),
            (
                "other".into(),
                ("b".repeat(64), "192.168.1.4:4212".parse().unwrap()),
            ),
        ]);
        assert_eq!(
            reconnect_address(&record, &discovered, 0)
                .unwrap()
                .to_string(),
            "192.168.1.3:4212"
        );
        assert_eq!(
            reconnect_address(&record, &discovered, 1)
                .unwrap()
                .to_string(),
            record.address
        );
        assert_eq!(
            reconnect_address(&record, &BTreeMap::new(), 0)
                .unwrap()
                .to_string(),
            record.address
        );
    }

    #[test]
    fn discovered_changed_address_resumes_without_pairing_and_updates_history() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Identity::load(&dir.path().join("desktop")).unwrap();
        let phone = Identity::load(&dir.path().join("phone")).unwrap();
        let fingerprint = desktop.fingerprint();
        let history = vec![ConnectionRecord {
            fingerprint: fingerprint.clone(),
            name: "Desktop".into(),
            auto_reconnect: true,
            address: "127.0.0.1:1".into(),
        }];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (mut receiver, _reader) = Receiver::start(phone, 0, 20, history).unwrap();
        receiver.update_discovered_desktop("desktop".into(), fingerprint.clone(), Some(address));
        let accepted = Arc::new(AtomicBool::new(false));
        let server_accepted = accepted.clone();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let socket = loop {
                if let Ok((socket, _)) = listener.accept() {
                    break socket;
                }
                assert!(
                    Instant::now() < deadline,
                    "automatic discovery reconnect timed out"
                );
                thread::sleep(Duration::from_millis(10));
            };
            let mut control = SecureControl::accept(socket, &desktop).unwrap();
            control
                .send(&Message::Hello {
                    name: "Desktop".into(),
                    protocol: 2,
                    control_port: Some(address.port()),
                })
                .unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Request {
                    intent: ConnectionIntent::Resume
                }
            ));
            control.send(&Message::Ready).unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Pending { .. }
            ));
            assert!(matches!(control.receive().unwrap(), Message::Accept { .. }));
            server_accepted.store(true, Ordering::Release);
            control
                .send(&Message::Stop {
                    session_id: control.session_id().into(),
                    reason: StopReason::UserDisconnect,
                })
                .unwrap();
            loop {
                match control.receive().unwrap() {
                    Message::Ping { .. } => continue,
                    Message::Stopped { .. } => break,
                    message => panic!("unexpected disconnect response: {message:?}"),
                }
            }
        });
        wait_for_timeout(Duration::from_secs(15), || accepted.load(Ordering::Acquire));
        wait_for(|| !receiver.history()[0].auto_reconnect);
        server.join().unwrap();
        assert_eq!(receiver.history()[0].address, address.to_string());
        assert_eq!(receiver.history()[0].fingerprint, fingerprint);
        receiver.update_discovered_desktop("desktop".into(), String::new(), None);
        assert!(receiver.discovered.lock().unwrap().is_empty());
        receiver.stop();
    }
    #[test]
    fn automatic_buffer_grows_on_glitches_and_recovers_without_overriding_manual_values() {
        let now = Instant::now();
        let mut adaptation = BufferAdaptation::new(20, now);
        assert_eq!(adaptation.update(20, true, 1, 0, now), 25);
        assert_eq!(adaptation.update(25, true, 1, 1, now), 30);
        assert_eq!(
            adaptation.update(30, true, 1, 1, now + Duration::from_secs(10)),
            29
        );
        assert_eq!(
            adaptation.update(8, false, 3, 4, now + Duration::from_secs(11)),
            8
        );
        assert_eq!(
            adaptation.update(8, false, 5, 8, now + Duration::from_secs(12)),
            8
        );
        assert_eq!(
            adaptation.update(100, true, 6, 9, now + Duration::from_secs(13)),
            100
        );
    }

    #[test]
    fn reconnect_failures_back_off_and_success_restores_quick_recovery() {
        let mut retry = ReconnectBackoff::default();
        for seconds in [3, 6, 12, 24, 30, 30] {
            assert_eq!(retry.after_attempt(false), Duration::from_secs(seconds));
        }
        assert_eq!(retry.after_attempt(true), Duration::from_secs(3));
        assert_eq!(retry.after_attempt(false), Duration::from_secs(3));
    }

    #[test]
    fn closing_and_disconnecting_keep_authorization_but_disconnect_disables_auto_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::load(&dir.path().join("identity")).unwrap();
        let record = ConnectionRecord {
            fingerprint: "trusted".into(),
            name: "Computer".into(),
            auto_reconnect: true,
            address: String::new(),
        };
        let (mut receiver, _reader) =
            Receiver::start(identity.clone(), 0, 20, vec![record]).unwrap();
        assert!(!receiver.snapshot().auto_buffer);
        receiver.state.lock().unwrap().fingerprint = "trusted".into();
        receiver.stop();
        let serialized = serde_json::to_vec(&receiver.history()).unwrap();
        let history = serde_json::from_slice(&serialized).unwrap();
        let (mut reopened, _reader) = Receiver::start(identity, 0, 20, history).unwrap();
        assert!(reopened.history()[0].auto_reconnect);
        reopened.state.lock().unwrap().fingerprint = "trusted".into();
        reopened.disconnect();
        assert!(!reopened.history()[0].auto_reconnect);
        reopened.stop();
        assert_eq!(reopened.history().len(), 1);
    }

    #[test]
    fn phone_initiates_authenticated_connection_adjusts_buffer_and_pauses_without_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Identity::load(&dir.path().join("desktop")).unwrap();
        let remembered_desktop = desktop.clone();
        let phone = Identity::load(&dir.path().join("phone")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (step, commands) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut control =
                SecureControl::accept(listener.accept().unwrap().0, &desktop).unwrap();
            control
                .send(&Message::Hello {
                    name: "Test computer".into(),
                    protocol: 2,
                    control_port: Some(address.port()),
                })
                .unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Request {
                    intent: ConnectionIntent::Manual
                }
            ));
            control.send(&Message::Ready).unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Pending { .. }
            ));
            let (key, port) = match control.receive().unwrap() {
                Message::Accept {
                    key, audio_port, ..
                } => (key, audio_port),
                response => panic!("{response:?}"),
            };
            let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
            let cipher = AudioCipher::new(&key);
            let mut sequence = 0;
            for _ in 0..2 {
                commands.recv_timeout(Duration::from_secs(3)).unwrap();
                for _ in 0..32 {
                    udp.send_to(
                        &cipher.seal(sequence, &vec![(20000, -20000); 120]).unwrap(),
                        ("127.0.0.1", port),
                    )
                    .unwrap();
                    sequence += 1;
                    thread::sleep(Duration::from_micros(2500));
                }
            }
            commands.recv_timeout(Duration::from_secs(3)).unwrap();
            control
                .send(&Message::Ping {
                    serial: 1,
                    rtt_ms: Some(2.0),
                    capture_period_ms: Some(10.0),
                })
                .unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Pong {
                    serial: 1,
                    buffer_ms: Some(25)
                }
            ));
            control
                .send(&Message::Stop {
                    session_id: control.session_id().to_owned(),
                    reason: StopReason::ReceiverClosed,
                })
                .unwrap();
        });
        let (mut receiver, mut reader) = Receiver::start(phone.clone(), 0, 20, Vec::new()).unwrap();
        receiver.connect(address, None).unwrap();
        wait_for(|| receiver.snapshot().state == "pending");
        assert_eq!(receiver.snapshot().peer_name, "Test computer");
        receiver.approve(true).unwrap();
        wait_for(|| !receiver.history().is_empty());
        assert_eq!(receiver.history()[0].address, address.to_string());
        receiver.set_buffer(25, false).unwrap();
        assert!(receiver.set_buffer(101, false).is_err());
        step.send(()).unwrap();
        wait_for(|| receiver.snapshot().received == 32);
        let mut output = [(0, 0); 192];
        reader.fill(&mut output);
        assert_eq!(output[100], (20000, -20000));
        assert!(receiver.snapshot().waveform.iter().any(|peak| *peak > 0.5));
        receiver.set_paused(true);
        reader.fill(&mut output);
        assert!(output.iter().all(|frame| *frame == (0, 0)));
        assert_eq!(receiver.snapshot().state, "paused");
        receiver.set_paused(false);
        step.send(()).unwrap();
        wait_for(|| receiver.snapshot().received == 64);
        reader.fill(&mut output);
        assert_eq!(output[100], (20000, -20000));
        assert_eq!(receiver.snapshot().lost, 0);
        assert_eq!(receiver.snapshot().underruns, 0);
        step.send(()).unwrap();
        server.join().unwrap();
        receiver.stop();
        assert!(receiver.history()[0].auto_reconnect);
        let mut history = receiver.history();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        history[0].address = address.to_string();
        history[0].auto_reconnect = false;
        let server = thread::spawn(move || {
            let mut control =
                SecureControl::accept(listener.accept().unwrap().0, &remembered_desktop).unwrap();
            control
                .send(&Message::Hello {
                    name: "Remembered computer".into(),
                    protocol: 2,
                    control_port: Some(address.port()),
                })
                .unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Request {
                    intent: ConnectionIntent::Manual
                }
            ));
            control.send(&Message::Ready).unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Pending { .. }
            ));
            // 关闭自动连接后手动重连，已允许的身份无需再次 approve。
            assert!(matches!(control.receive().unwrap(), Message::Accept { .. }));
            control
                .send(&Message::Stop {
                    session_id: control.session_id().to_owned(),
                    reason: StopReason::ReceiverClosed,
                })
                .unwrap();
        });
        let (mut reopened, _reader) = Receiver::start(phone.clone(), 0, 20, history).unwrap();
        reopened
            .connect(address, Some(receiver.history()[0].fingerprint.clone()))
            .unwrap();
        server.join().unwrap();
        reopened.stop();
        assert_eq!(reopened.history()[0].name, "Remembered computer");
        assert!(!reopened.history()[0].auto_reconnect);
        let mut history = reopened.history();
        history[0].auto_reconnect = true;
        let impostor = Identity::load(&dir.path().join("different-desktop")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let impostor_address = listener.local_addr().unwrap();
        let expected = history[0].fingerprint.clone();
        history[0].address = "127.0.0.1:1".into();
        let server = thread::spawn(move || {
            let mut control =
                SecureControl::accept(listener.accept().unwrap().0, &impostor).unwrap();
            assert!(control.receive().is_err());
        });
        let (mut guarded, _reader) = Receiver::start(phone, 0, 20, history).unwrap();
        guarded.update_discovered_desktop(
            "spoofed-discovery".into(),
            expected,
            Some(impostor_address),
        );
        wait_for_timeout(Duration::from_secs(15), || {
            guarded.snapshot().error.contains("身份已改变")
        });
        server.join().unwrap();
        guarded.stop();
        assert_eq!(guarded.history()[0].name, "Remembered computer");
    }

    #[test]
    fn remote_disconnect_is_acknowledged_and_stops_automatic_retries() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Identity::load(&dir.path().join("desktop")).unwrap();
        let phone = Identity::load(&dir.path().join("phone")).unwrap();
        let (mut receiver, _reader) = Receiver::start(phone, 0, 20, Vec::new()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut control =
                SecureControl::accept(listener.accept().unwrap().0, &desktop).unwrap();
            control
                .send(&Message::Hello {
                    name: "Desktop".into(),
                    protocol: 2,
                    control_port: Some(address.port()),
                })
                .unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Request {
                    intent: ConnectionIntent::Manual
                }
            ));
            control.send(&Message::Ready).unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Pending { .. }
            ));
            assert!(matches!(control.receive().unwrap(), Message::Accept { .. }));
            let session_id = control.session_id().to_owned();
            control
                .send(&Message::Stop {
                    session_id: session_id.clone(),
                    reason: StopReason::UserDisconnect,
                })
                .unwrap();
            assert!(
                matches!(control.receive().unwrap(), Message::Stopped { session_id: response } if response == session_id)
            );
            listener
        });
        receiver.connect(address, None).unwrap();
        wait_for(|| receiver.snapshot().state == "pending");
        receiver.approve(true).unwrap();
        let listener = server.join().unwrap();
        wait_for(|| receiver.snapshot().state == "waiting");
        assert_eq!(receiver.history().len(), 1);
        assert!(!receiver.history()[0].auto_reconnect);
        listener.set_nonblocking(true).unwrap();
        // 超过首次重试窗口后仍没有连接，验证协议结果而非仅检查字段。
        thread::sleep(Duration::from_millis(3200));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        receiver.stop();
    }

    #[test]
    fn blocked_resume_is_acknowledged_before_audio_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Identity::load(&dir.path().join("desktop")).unwrap();
        let phone = Identity::load(&dir.path().join("phone")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for intent in [ConnectionIntent::Manual, ConnectionIntent::Resume] {
                let mut control =
                    SecureControl::accept(listener.accept().unwrap().0, &desktop).unwrap();
                control
                    .send(&Message::Hello {
                        name: "Desktop".into(),
                        protocol: 2,
                        control_port: Some(address.port()),
                    })
                    .unwrap();
                assert!(
                    matches!(control.receive().unwrap(), Message::Request { intent: actual } if actual == intent)
                );
                if intent == ConnectionIntent::Manual {
                    control.send(&Message::Ready).unwrap();
                    assert!(matches!(
                        control.receive().unwrap(),
                        Message::Pending { .. }
                    ));
                    assert!(matches!(control.receive().unwrap(), Message::Accept { .. }));
                    // 直接关闭传输，模拟断网；手机仍应发起恢复请求。
                } else {
                    let session_id = control.session_id().to_owned();
                    control
                        .send(&Message::Stop {
                            session_id: session_id.clone(),
                            reason: StopReason::UserDisconnect,
                        })
                        .unwrap();
                    assert!(
                        matches!(control.receive().unwrap(), Message::Stopped { session_id: response } if response == session_id)
                    );
                }
            }
        });
        let (mut receiver, _reader) = Receiver::start(phone, 0, 20, Vec::new()).unwrap();
        receiver.connect(address, None).unwrap();
        wait_for(|| receiver.snapshot().state == "pending");
        receiver.approve(true).unwrap();
        server.join().unwrap();
        wait_for(|| receiver.snapshot().state == "waiting");
        assert_eq!(receiver.history().len(), 1);
        assert!(!receiver.history()[0].auto_reconnect);
        assert_eq!(receiver.snapshot().received, 0);
        receiver.stop();
    }

    #[test]
    fn background_buffer_restores_user_settings_and_keeps_its_floor_during_adaptation() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::load(&dir.path().join("phone")).unwrap();
        let (mut receiver, _reader) = Receiver::start(identity, 0, 20, Vec::new()).unwrap();
        receiver.set_background(true);
        assert_eq!(receiver.snapshot().buffer_ms, 50);
        assert_eq!(receiver.snapshot().buffer_floor_ms, 20);
        receiver.set_buffer(10, false).unwrap();
        assert_eq!(receiver.snapshot().buffer_ms, 50);
        let now = Instant::now();
        let mut adaptation = BufferAdaptation::new(50, now);
        assert_eq!(adaptation.update(50, true, 1, 0, now), 55);
        assert_eq!(
            adaptation.update(55, true, 1, 0, now + Duration::from_secs(10)),
            54
        );
        assert_eq!(adaptation.floor, 50);
        receiver.set_background(false);
        assert_eq!(receiver.snapshot().buffer_ms, 10);
        assert!(!receiver.snapshot().auto_buffer);
        receiver.stop();
    }

    #[test]
    fn phone_disconnect_notifies_desktop_and_keeps_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = Identity::load(&dir.path().join("desktop")).unwrap();
        let phone = Identity::load(&dir.path().join("phone")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, actions) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut control =
                SecureControl::accept(listener.accept().unwrap().0, &desktop).unwrap();
            control
                .send(&Message::Hello {
                    name: "Desktop".into(),
                    protocol: 2,
                    control_port: Some(address.port()),
                })
                .unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Request {
                    intent: ConnectionIntent::Manual
                }
            ));
            control.send(&Message::Ready).unwrap();
            assert!(matches!(
                control.receive().unwrap(),
                Message::Pending { .. }
            ));
            assert!(matches!(control.receive().unwrap(), Message::Accept { .. }));
            actions.recv_timeout(Duration::from_secs(3)).unwrap();
            control
                .send(&Message::Ping {
                    serial: 0,
                    rtt_ms: None,
                    capture_period_ms: None,
                })
                .unwrap();
            let session_id = control.session_id().to_owned();
            loop {
                match control.receive().unwrap() {
                    Message::Pong { serial: 0, .. } => {}
                    Message::Stop {
                        session_id: response,
                        reason: StopReason::UserDisconnect,
                    } => {
                        assert_eq!(response, session_id);
                        control.send(&Message::Stopped { session_id }).unwrap();
                        break;
                    }
                    other => panic!("unexpected disconnect response: {other:?}"),
                }
            }
        });
        let (mut receiver, _reader) = Receiver::start(phone, 0, 20, Vec::new()).unwrap();
        receiver.connect(address, None).unwrap();
        wait_for(|| receiver.snapshot().state == "pending");
        receiver.approve(true).unwrap();
        wait_for(|| !receiver.history().is_empty());
        receiver.disconnect();
        ready.send(()).unwrap();
        server.join().unwrap();
        wait_for(|| receiver.snapshot().state == "waiting");
        assert_eq!(receiver.history().len(), 1);
        assert!(!receiver.history()[0].auto_reconnect);
        receiver.stop();
    }
    fn wait_for(mut condition: impl FnMut() -> bool) {
        wait_for_timeout(Duration::from_secs(3), &mut condition);
    }

    fn wait_for_timeout(timeout: Duration, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "receiver did not reach the expected state"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn playback() -> (Producer<StereoFrame>, PlaybackReader) {
        let (producer, consumer) = RingBuffer::new(9600);
        let stats = Arc::new(PlaybackStats {
            target_frames: AtomicUsize::new(480),
            ..Default::default()
        });
        stats.active.store(true, Ordering::Relaxed);
        (
            producer,
            PlaybackReader {
                consumer,
                stats,
                was_active: false,
                primed: false,
                last_frame: (0, 0),
                fade_from: (0, 0),
                fade_remaining: 0,
                fade_out_remaining: 0,
            },
        )
    }
    #[test]
    fn ten_ms_packet_batches_supply_four_ms_callbacks_without_underruns() {
        let (mut producer, mut reader) = playback();
        let mut output = [(0, 0); 192];
        let mut started = false;
        for micros in (0..1_000_000).step_by(500) {
            if micros % 10_000 == 0 {
                for _ in 0..480 {
                    producer.push((12000, -12000)).unwrap();
                }
            }
            if micros % 4_000 == 0 {
                reader.fill(&mut output);
                if started {
                    assert!(output.iter().all(|frame| *frame == (12000, -12000)));
                }
                started |= reader.stats.started.load(Ordering::Relaxed);
            }
        }
        assert!(started);
        assert_eq!(reader.stats.underruns.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn underrun_fades_across_callback_boundary_and_rebuffers_before_restart() {
        let (mut producer, mut reader) = playback();
        for _ in 0..958 {
            producer.push((30000, 30000)).unwrap();
        }
        let mut output = [(0, 0); 192];
        for _ in 0..4 {
            reader.fill(&mut output);
        }
        // 190 frames remain: the final two start a fade that crosses callbacks.
        reader.fill(&mut output);
        assert!(
            output
                .windows(2)
                .all(|pair| (pair[1].0 as i32 - pair[0].0 as i32).abs() < 3000)
        );
        assert!(output[191].0 > 0);
        reader.fill(&mut output);
        assert!(output[0].0 > 0);
        assert_eq!(output[48], (0, 0));
        assert!(!reader.stats.started.load(Ordering::Relaxed));
        for _ in 0..720 {
            producer.push((-30000, -30000)).unwrap();
        }
        reader.fill(&mut output);
        assert_eq!(output[0], (0, 0));
        assert_eq!(output[49], (-30000, -30000));
        assert_eq!(reader.stats.underruns.load(Ordering::Relaxed), 1);
    }
}
