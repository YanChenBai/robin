use crate::engine::{CaptureDevice, DesktopSnapshot};
use anyhow::Result;
use robin_core::{
    FRAMES_PER_PACKET, SAMPLE_RATE, StereoFrame,
    audio::{AudioCipher, PacketPacer},
};
use std::{
    collections::VecDeque,
    net::UdpSocket,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}
struct TimerResolution;
impl TimerResolution {
    fn new() -> Result<Self> {
        anyhow::ensure!(
            unsafe { timeBeginPeriod(1) } == 0,
            "无法启用音频发送高精度计时"
        );
        Ok(Self)
    }
}
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe { timeEndPeriod(1) };
    }
}

pub fn devices() -> Result<Vec<CaptureDevice>> {
    // GPUI owns an STA thread; WASAPI enumeration needs its own COM apartment.
    std::thread::spawn(enumerate_devices)
        .join()
        .map_err(|_| anyhow::anyhow!("音频设备枚举线程异常退出"))?
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

fn enumerate_devices() -> Result<Vec<CaptureDevice>> {
    wasapi::initialize_mta().ok()?;
    let _apartment = ComApartment;
    let enumerator = DeviceEnumerator::new()?;
    let default_id = enumerator
        .get_default_device(&Direction::Render)
        .ok()
        .and_then(|device| device.get_id().ok());
    let collection = enumerator.get_device_collection(&Direction::Render)?;
    let mut devices = Vec::new();
    for ix in 0..collection.get_nbr_devices()? {
        let device = collection.get_device_at_index(ix)?;
        let id = device.get_id()?;
        devices.push(CaptureDevice {
            name: device.get_friendlyname()?,
            is_default: Some(&id) == default_id.as_ref(),
            id,
        });
    }
    Ok(devices)
}

pub fn smoke() -> Result<()> {
    use std::{sync::atomic::AtomicU64, thread, time::Duration};
    // Match GPUI's apartment when checking the device picker regression.
    wasapi::initialize_sta().ok()?;
    let _apartment = ComApartment;
    let endpoints = devices()?;
    println!(
        "Enumerated {} output devices from an STA caller",
        endpoints.len()
    );
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    receiver.set_read_timeout(Some(Duration::from_secs(4)))?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    sender.connect(receiver.local_addr()?)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let count = Arc::new(AtomicU64::new(0));
    let total = count.clone();
    let listener = thread::spawn(move || {
        let cipher = AudioCipher::new(&[7; 32]);
        let mut playout = robin_core::audio::PlayoutBuffer::new(10)?;
        let mut bytes = [0; 1200];
        let mut first = None;
        let result = (|| -> Result<(f64, u64, u64)> {
            loop {
                let (len, _) = receiver.recv_from(&mut bytes)?;
                let now = Instant::now();
                let start = *first.get_or_insert(now);
                let (sequence, frames) = cipher.open(&bytes[..len])?;
                playout.push(sequence, frames, now);
                while playout.pop_due(now).is_some() {}
                total.fetch_add(1, Ordering::Relaxed);
                if now.duration_since(start) >= Duration::from_secs(2) {
                    return Ok((
                        total.load(Ordering::Relaxed) as f64
                            / now.duration_since(start).as_secs_f64(),
                        playout.lost(),
                        playout.late(),
                    ));
                }
            }
        })();
        flag.store(true, Ordering::Release);
        result
    });
    let state = Arc::new(Mutex::new(DesktopSnapshot::default()));
    let capture_stop = stop.clone();
    let capture_state = state.clone();
    let result = thread::spawn(move || {
        run(
            sender,
            [7; 32],
            capture_stop,
            Arc::new(Mutex::new(String::new())),
            capture_state,
        )
    })
    .join()
    .map_err(|_| anyhow::anyhow!("smoke capture panicked"))?;
    stop.store(true, Ordering::Release);
    let (rate, lost, late) = listener
        .join()
        .map_err(|_| anyhow::anyhow!("smoke listener panicked"))??;
    result?;
    println!(
        "Packet cadence {rate:.1} packets/s (target 400); local 10 ms reorder window: missing {lost}, late {late}"
    );
    let snapshot = state.lock().unwrap();
    println!(
        "Captured '{}', WASAPI period {:.2} ms; {} authenticated PCM packets received",
        snapshot.capture_name,
        snapshot.capture_period_ms,
        count.load(Ordering::Relaxed)
    );
    Ok(())
}

pub fn run(
    socket: UdpSocket,
    key: [u8; 32],
    stop: Arc<AtomicBool>,
    selected: Arc<Mutex<String>>,
    state: Arc<Mutex<DesktopSnapshot>>,
) -> Result<()> {
    wasapi::initialize_mta().ok()?;
    let _apartment = ComApartment;
    let cipher = AudioCipher::new(&key);
    let _timer_resolution = TimerResolution::new()?;
    let mut pacer = PacketPacer::new(Instant::now());
    while !stop.load(Ordering::Acquire) {
        let id = selected.lock().unwrap().clone();
        let enumerator = DeviceEnumerator::new()?;
        let device = if id.is_empty() {
            enumerator.get_default_device(&Direction::Render)?
        } else {
            enumerator.get_device(&id)?
        };
        let active_id = device.get_id()?;
        let mut client = device.get_iaudioclient()?;
        let format = WaveFormat::new(16, 16, &SampleType::Int, SAMPLE_RATE as usize, 2, None);
        let (period, minimum) = client.get_device_period()?;
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: minimum,
            },
        )?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        {
            let mut snapshot = state.lock().unwrap();
            snapshot.capture_name = device.get_friendlyname()?;
            snapshot.capture_period_ms = period as f64 / 10_000.0;
        }
        let mut queue = VecDeque::with_capacity(48_000 * 4 / 10);
        let mut last_capture = Instant::now();
        let mut check_default_at = Instant::now() + std::time::Duration::from_secs(1);
        let source_period = std::time::Duration::from_nanos(period.max(1) as u64 * 100);
        let result = (|| -> Result<()> {
            while !stop.load(Ordering::Acquire) && *selected.lock().unwrap() == id {
                if id.is_empty() && Instant::now() >= check_default_at {
                    check_default_at = Instant::now() + std::time::Duration::from_secs(1);
                    if let Ok(default) = enumerator.get_default_device(&Direction::Render)
                        && default.get_id()? != active_id
                    {
                        break;
                    }
                }
                while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
                    capture.read_from_device_to_deque(&mut queue)?;
                    last_capture = Instant::now();
                }
                while queue.len() > 48_000 * 4 / 50 {
                    for _ in 0..4 {
                        queue.pop_front();
                    }
                }
                let now = Instant::now();
                for _ in 0..8 {
                    // WASAPI supplies batches. Preserve partial PCM until the next batch.
                    if queue.len() < FRAMES_PER_PACKET * 4
                        && now.duration_since(last_capture) < source_period * 2
                    {
                        break;
                    }
                    let Some(sequence) = pacer.take_due(now)? else {
                        break;
                    };
                    // Pace UDP even when the loopback endpoint is silent.
                    let mut frames: [StereoFrame; FRAMES_PER_PACKET] = [(0, 0); FRAMES_PER_PACKET];
                    for frame in &mut frames {
                        if queue.len() < 4 {
                            break;
                        }
                        let left = i16::from_le_bytes([
                            queue.pop_front().unwrap(),
                            queue.pop_front().unwrap(),
                        ]);
                        let right = i16::from_le_bytes([
                            queue.pop_front().unwrap(),
                            queue.pop_front().unwrap(),
                        ]);
                        *frame = (left, right);
                    }
                    socket.send(&cipher.seal(sequence, &frames)?)?;
                    let peak = frames
                        .iter()
                        .map(|(left, right)| {
                            (*left as i32)
                                .unsigned_abs()
                                .max((*right as i32).unsigned_abs())
                        })
                        .max()
                        .unwrap_or(0);
                    let mut snapshot = state.lock().unwrap();
                    snapshot.sent += 1;
                    snapshot.audio_level = peak as f32 / 32768.0;
                }
                let _ = event.wait_for_event(1);
            }
            Ok(())
        })();
        let stopped = client.stop_stream();
        // 设备切换时旧端点可能已经失效，不把正常切换当作会话失败。
        let source_changed = *selected.lock().unwrap() != id
            || (id.is_empty()
                && enumerator
                    .get_default_device(&Direction::Render)
                    .and_then(|device| device.get_id())
                    .is_ok_and(|current| current != active_id));
        if source_changed {
            continue;
        }
        stopped?;
        result?;
    }
    Ok(())
}
