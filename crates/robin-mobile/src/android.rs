use crate::history;
use anyhow::Result;
use jni::{
    JNIEnv,
    objects::{JClass, JString},
    sys::{jboolean, jint, jstring},
};
use oboe::{
    AudioOutputCallback, AudioOutputStreamSafe, AudioStream, AudioStreamAsync, AudioStreamBase,
    AudioStreamBuilder, AudioStreamSafe, DataCallbackResult, Output, PerformanceMode, SharingMode,
    Stereo, Usage,
};
use robin_core::{
    control::Identity,
    receiver::{ConnectionRecord, PlaybackReader, Receiver},
};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    sync::{Mutex, OnceLock},
};

struct Callback(PlaybackReader);
impl AudioOutputCallback for Callback {
    type FrameType = (i16, Stereo);
    fn on_audio_ready(
        &mut self,
        _: &mut dyn AudioOutputStreamSafe,
        output: &mut [(i16, i16)],
    ) -> DataCallbackResult {
        self.0.fill(output);
        DataCallbackResult::Continue
    }
    fn on_error_after_close(&mut self, _: &mut dyn AudioOutputStreamSafe, _: oboe::Error) {
        self.0.stats().failed.store(true, Ordering::Release);
    }
}

struct Runtime {
    receiver: Receiver,
    stream: AudioStreamAsync<Output, Callback>,
    history_path: PathBuf,
}
// The audio stream is kept on its creating worker thread; JNI communicates with that worker.
enum Command {
    Snapshot(std::sync::mpsc::Sender<String>),
    Approve,
    Reject,
    Disconnect,
    AutoConnect(
        bool,
        std::sync::mpsc::Sender<std::result::Result<(), String>>,
    ),
    DiscoveredDesktop(String, String, Option<std::net::SocketAddr>),
    Connect(String, Option<String>),
    Buffer(u64, u64, bool),
    Pause(bool),
    Background(bool),
    Stop,
}
struct Handle {
    sender: std::sync::mpsc::Sender<Command>,
    worker: std::thread::JoinHandle<()>,
}
static HANDLE: OnceLock<Mutex<Option<Handle>>> = OnceLock::new();
fn handle() -> &'static Mutex<Option<Handle>> {
    HANDLE.get_or_init(|| Mutex::new(None))
}

fn start(path: PathBuf, buffer_ms: u64, background_buffer_ms: u64, automatic: bool) -> Result<String> {
    // 启停和离线设置共用句柄锁，避免旧服务退出时覆盖刚保存的偏好。
    let mut current = handle().lock().unwrap();
    anyhow::ensure!(current.is_none(), "receiver already started");
    let (sender, commands) = std::sync::mpsc::channel();
    let (ready, started) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("robin-android".into())
        .spawn(move || {
            let result = (|| -> Result<Runtime> {
                let identity = Identity::load(&path.join("identity.key"))?;
                let history_path = path.join("connections.json");
                let history = history::load(&history_path)?;
                let auto_connect = history::load_auto_connect(&path.join("preferences.json"))?;
                let (receiver, reader) =
                    Receiver::start(identity, 4211, buffer_ms, background_buffer_ms, history, auto_connect)?;
                receiver.set_buffer(buffer_ms, background_buffer_ms, automatic)?;
                receiver.set_paused(true);
                let stats = reader.stats().clone();
                let mut stream = AudioStreamBuilder::default()
                    .set_output()
                    .set_stereo()
                    .set_i16()
                    .set_sample_rate(48_000)
                    .set_performance_mode(PerformanceMode::LowLatency)
                    .set_sharing_mode(SharingMode::Exclusive)
                    .set_usage(Usage::Media)
                    .set_sample_rate_conversion_quality(oboe::SampleRateConversionQuality::Medium)
                    .set_callback(Callback(reader))
                    .open_stream()?;
                let burst = stream.get_frames_per_burst();
                let frames = stream.set_buffer_size_in_frames(burst * 2)?;
                stats.output_frames.store(frames as u64, Ordering::Relaxed);
                anyhow::ensure!(
                    stream.get_sample_rate() == 48_000,
                    "unsupported negotiated output sample rate"
                );
                stream.request_start()?;
                Ok(Runtime {
                    receiver,
                    stream,
                    history_path,
                })
            })();
            let mut runtime = match result {
                Ok(runtime) => {
                    let _ = ready.send(Ok(
                        serde_json::to_string(&runtime.receiver.snapshot()).unwrap()
                    ));
                    runtime
                }
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            let mut saved = String::new();
            let mut xruns = 0;
            loop {
                let command = match commands.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(command) => Some(command),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(_) => Some(Command::Stop),
                };
                let mut stopping = false;
                match command {
                    Some(Command::Snapshot(reply)) => {
                        let _ = reply
                            .send(serde_json::to_string(&runtime.receiver.snapshot()).unwrap());
                    }
                    Some(Command::Approve) => {
                        let _ = runtime.receiver.approve();
                    }
                    Some(Command::Reject) => {
                        let _ = runtime.receiver.reject();
                    }
                    Some(Command::Disconnect) => runtime.receiver.disconnect(),
                    Some(Command::AutoConnect(enabled, reply)) => {
                        let result = (|| -> Result<()> {
                            history::save_auto_connect(
                                &runtime.history_path.with_file_name("preferences.json"),
                                enabled,
                            )?;
                            runtime.receiver.set_auto_connect(enabled);
                            Ok(())
                        })();
                        let _ = reply.send(result.map_err(|error| error.to_string()));
                    }
                    Some(Command::DiscoveredDesktop(service, fingerprint, address)) => {
                        runtime
                            .receiver
                            .update_discovered_desktop(service, fingerprint, address);
                    }
                    Some(Command::Stop) => {
                        runtime.receiver.stop();
                        stopping = true;
                    }
                    Some(Command::Connect(address, fingerprint)) => {
                        let result = address
                            .parse()
                            .map_err(anyhow::Error::from)
                            .and_then(|address| runtime.receiver.connect(address, fingerprint));
                        if let Err(error) = result {
                            runtime.receiver.report_error(error.to_string());
                        }
                    }
                    Some(Command::Buffer(buffer_ms, background_buffer_ms, automatic)) => {
                        if let Err(error) = runtime.receiver.set_buffer(buffer_ms, background_buffer_ms, automatic) {
                            runtime.receiver.report_error(error.to_string());
                        }
                    }
                    Some(Command::Pause(paused)) => runtime.receiver.set_paused(paused),
                    Some(Command::Background(background)) => {
                        runtime.receiver.set_background(background)
                    }
                    None => {}
                }
                // 输出设备欠载和网络欠载分别处理，按硬件 burst 逐步增大输出缓冲。
                if let Ok(count) = runtime.stream.get_xrun_count() {
                    if count > xruns {
                        let frames = runtime.stream.get_buffer_size_in_frames()
                            + runtime.stream.get_frames_per_burst();
                        if let Ok(frames) = runtime.stream.set_buffer_size_in_frames(
                            frames.min(runtime.stream.get_buffer_capacity_in_frames()),
                        ) {
                            runtime.receiver.stats_output_frames(frames as u64);
                        }
                    }
                    xruns = count;
                }
                if let Ok(history) = serde_json::to_string(&runtime.receiver.history()) {
                    if history != saved {
                        match history::save(&runtime.history_path, &runtime.receiver.history()) {
                            Ok(contents) => saved = contents,
                            Err(error) => runtime
                                .receiver
                                .report_error(format!("无法保存电脑记录：{error}")),
                        }
                    }
                }
                if stopping {
                    break;
                }
            }
        })?;
    match started.recv_timeout(std::time::Duration::from_secs(10))? {
        Ok(snapshot) => {
            *current = Some(Handle { sender, worker });
            Ok(snapshot)
        }
        Err(error) => {
            let _ = worker.join();
            anyhow::bail!(error)
        }
    }
}

fn command(command: Command) {
    // 启停期间不阻塞页面快照；接收服务尚未就绪时仍读本地记录。
    if let Ok(current) = handle().try_lock()
        && let Some(handle) = current.as_ref()
    {
        let _ = handle.sender.send(command);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeDiscoveredDesktop(
    mut env: JNIEnv,
    _: JClass,
    service: JString,
    fingerprint: JString,
    address: JString,
) {
    if let (Ok(service), Ok(fingerprint), Ok(address)) = (
        env.get_string(&service),
        env.get_string(&fingerprint),
        env.get_string(&address),
    ) {
        let address: String = address.into();
        command(Command::DiscoveredDesktop(
            service.into(),
            fingerprint.into(),
            address.parse().ok(),
        ));
    }
}
fn text(env: &mut JNIEnv, value: String) -> jstring {
    env.new_string(value)
        .map(|v| v.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeStart(
    mut env: JNIEnv,
    _: JClass,
    path: JString,
    buffer_ms: jint,
    background_buffer_ms: jint,
    automatic: jboolean,
) -> jstring {
    let result = (|| -> Result<String> {
        let path: String = env.get_string(&path)?.into();
        start(PathBuf::from(path), buffer_ms as u64, background_buffer_ms as u64, automatic != 0)
    })();
    let value = result
        .unwrap_or_else(|e| serde_json::json!({"state":"error","error":e.to_string()}).to_string());
    text(&mut env, value)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeSnapshot(
    mut env: JNIEnv,
    _: JClass,
    path: JString,
) -> jstring {
    let directory: String = env.get_string(&path).map(|s| s.into()).unwrap_or_default();
    let (sender, receiver) = std::sync::mpsc::channel();
    command(Command::Snapshot(sender));
    let value = receiver
        .recv_timeout(std::time::Duration::from_millis(100))
        .unwrap_or_else(|_| {
            let history = std::fs::read(PathBuf::from(&directory).join("connections.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<ConnectionRecord>>(&bytes).ok())
                .unwrap_or_default();
            let auto_connect = history::load_auto_connect(&PathBuf::from(directory).join("preferences.json"));
            match auto_connect {
                Ok(enabled) => serde_json::json!({"state":"stopped", "history":history, "autoConnect":enabled}).to_string(),
                Err(error) => serde_json::json!({"state":"error", "history":history, "error":error.to_string()}).to_string(),
            }
        });
    text(&mut env, value)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeApprove(
    _: JNIEnv,
    _: JClass,
) {
    command(Command::Approve);
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeReject(_: JNIEnv, _: JClass) {
    command(Command::Reject);
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeDisconnect(_: JNIEnv, _: JClass) {
    command(Command::Disconnect);
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeSetAutoConnect(
    mut env: JNIEnv,
    _: JClass,
    directory: JString,
    enabled: jboolean,
) {
    let result = (|| -> Result<()> {
        let directory: String = env.get_string(&directory)?.into();
        let current = handle().lock().unwrap();
        if let Some(current) = current.as_ref() {
            let (reply, response) = std::sync::mpsc::channel();
            current
                .sender
                .send(Command::AutoConnect(enabled != 0, reply))?;
            response
                .recv_timeout(std::time::Duration::from_secs(2))?
                .map_err(anyhow::Error::msg)?;
        } else {
            history::save_auto_connect(
                &PathBuf::from(directory).join("preferences.json"),
                enabled != 0,
            )?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = env.throw_new("java/lang/IllegalStateException", error.to_string());
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeStop(_: JNIEnv, _: JClass) {
    let mut current = handle().lock().unwrap();
    let stopped = current.take();
    if let Some(handle) = stopped {
        let _ = handle.sender.send(Command::Stop);
        let _ = handle.worker.join();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeConnect(
    mut env: JNIEnv,
    _: JClass,
    address: JString,
    fingerprint: JString,
) {
    if let (Ok(address), Ok(fingerprint)) = (env.get_string(&address), env.get_string(&fingerprint))
    {
        let fingerprint: String = fingerprint.into();
        command(Command::Connect(
            address.into(),
            if fingerprint.is_empty() {
                None
            } else {
                Some(fingerprint)
            },
        ));
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeBuffer(
    _: JNIEnv,
    _: JClass,
    buffer_ms: jint,
    background_buffer_ms: jint,
    automatic: jboolean,
) {
    command(Command::Buffer(buffer_ms as u64, background_buffer_ms as u64, automatic != 0));
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativePause(
    _: JNIEnv,
    _: JClass,
    paused: jboolean,
) {
    command(Command::Pause(paused != 0));
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_robin_audio_RobinAudio_nativeBackground(
    _: JNIEnv,
    _: JClass,
    background: jboolean,
) {
    command(Command::Background(background != 0));
}
