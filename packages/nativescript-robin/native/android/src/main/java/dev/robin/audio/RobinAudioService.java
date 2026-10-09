package dev.robin.audio;

import android.app.Notification;
import android.app.Activity;
import android.app.ActivityManager;
import android.app.Application;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.media.MediaMetadata;
import android.media.session.MediaSession;
import android.media.session.PlaybackState;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import java.util.UUID;
import java.util.ArrayDeque;
import java.util.HashSet;
import java.nio.charset.StandardCharsets;
import java.net.Inet4Address;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import org.json.JSONObject;

public final class RobinAudioService extends Service {
    private final ScheduledExecutorService worker = Executors.newSingleThreadScheduledExecutor();
    private final Handler main = new Handler(Looper.getMainLooper());
    private NsdManager nsd;
    private NsdManager.RegistrationListener registration;
    private NsdManager.DiscoveryListener discovery;
    private final ArrayDeque<NsdServiceInfo> resolveQueue = new ArrayDeque<>();
    private final HashSet<String> discoveredServices = new HashSet<>();
    private boolean resolving;
    private PowerManager.WakeLock wakeLock;
    private WifiManager.WifiLock wifiLock;
    private WifiManager.WifiLock backgroundWifiLock;
    private Bitmap artwork;
    private volatile boolean background;
    private final Application.ActivityLifecycleCallbacks activityLifecycle = new Application.ActivityLifecycleCallbacks() {
        @Override public void onActivityResumed(Activity activity) { setBackground(false); }
        @Override public void onActivityPaused(Activity activity) { setBackground(true); }
        @Override public void onActivityCreated(Activity activity, Bundle state) {}
        @Override public void onActivityStarted(Activity activity) {}
        @Override public void onActivityStopped(Activity activity) {}
        @Override public void onActivitySaveInstanceState(Activity activity, Bundle state) {}
        @Override public void onActivityDestroyed(Activity activity) {}
    };
    private AudioManager audio;
    private AudioFocusRequest focus;
    private MediaSession media;
    private boolean started;
    private volatile boolean destroyed;
    private boolean hasFocus;
    private boolean focusRequested;
    private boolean userPaused;
    private String lastNotification = "";

    @Override public void onCreate() {
        super.onCreate();
        RobinAudio.initialize(this);
        ActivityManager.RunningAppProcessInfo process = new ActivityManager.RunningAppProcessInfo();
        ActivityManager.getMyMemoryState(process);
        background = process.importance != ActivityManager.RunningAppProcessInfo.IMPORTANCE_FOREGROUND;
        getApplication().registerActivityLifecycleCallbacks(activityLifecycle);
        Drawable appIcon = getApplicationInfo().loadIcon(getPackageManager());
        artwork = Bitmap.createBitmap(256, 256, Bitmap.Config.ARGB_8888);
        appIcon.setBounds(0, 0, 256, 256);
        appIcon.draw(new Canvas(artwork));
        getSystemService(NotificationManager.class).createNotificationChannel(
            new NotificationChannel("robin-audio", "知更鸟音频", NotificationManager.IMPORTANCE_LOW));
        audio = getSystemService(AudioManager.class);
        focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
            .setAcceptsDelayedFocusGain(true).setWillPauseWhenDucked(true)
            .setOnAudioFocusChangeListener(change -> {
                hasFocus = change == AudioManager.AUDIOFOCUS_GAIN;
                if (change == AudioManager.AUDIOFOCUS_LOSS) {
                    userPaused = true;
                    focusRequested = false;
                    audio.abandonAudioFocusRequest(focus);
                }
                RobinAudio.nativePause(!hasFocus || userPaused);
            }, main).build();
        media = new MediaSession(this, "Robin");
        media.setCallback(new MediaSession.Callback() {
            @Override public void onPlay() { play(); }
            @Override public void onPause() { pause(); }
            @Override public void onStop() { stopSelf(); }
        }, main);
        media.setPlaybackToLocal(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build());
        media.setActive(true);
        startForeground(4211, notification("接收已开启", false));
        wakeLock = getSystemService(PowerManager.class).newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "Robin:Audio");
        wakeLock.acquire();
        WifiManager wifi = (WifiManager) getApplicationContext().getSystemService(WIFI_SERVICE);
        if (wifi != null) {
            wifiLock = wifi.createWifiLock(Build.VERSION.SDK_INT >= 29 ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY : WifiManager.WIFI_MODE_FULL_HIGH_PERF, "Robin:Audio");
            wifiLock.acquire();
            // Android 14 起高性能锁也退化为前台低延迟锁，后台由缓冲策略补偿。
            if (Build.VERSION.SDK_INT >= 29 && Build.VERSION.SDK_INT < 34) {
                backgroundWifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "Robin:BackgroundAudio");
                backgroundWifiLock.acquire();
            }
        }
    }

    private void play() {
        userPaused = false;
        int result = hasFocus ? AudioManager.AUDIOFOCUS_REQUEST_GRANTED : audio.requestAudioFocus(focus);
        focusRequested = result != AudioManager.AUDIOFOCUS_REQUEST_FAILED;
        hasFocus = result == AudioManager.AUDIOFOCUS_REQUEST_GRANTED;
        RobinAudio.nativePause(!hasFocus);
        if (!focusRequested) RobinAudio.serviceError = "无法获取音频焦点，请稍后点击继续播放";
        else RobinAudio.serviceError = "";
    }

    private void setBackground(boolean value) {
        background = value;
        if (started && !destroyed) worker.execute(() -> RobinAudio.nativeBackground(background));
    }

    private void pause() {
        userPaused = true;
        RobinAudio.nativePause(true);
        audio.abandonAudioFocusRequest(focus);
        hasFocus = false;
        focusRequested = false;
    }

    @Override public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? "" : intent.getAction();
        if ("stop".equals(action)) { stopSelf(); return START_NOT_STICKY; }
        if (destroyed) return START_NOT_STICKY;
        if (!started) {
            started = true;
            final int bufferMs = intent == null ? 20 : intent.getIntExtra("bufferMs", 20);
            final int backgroundBufferMs = intent == null ? 20 : intent.getIntExtra("backgroundBufferMs", 20);
            final boolean automatic = intent != null && intent.getBooleanExtra("automatic", false);
            worker.execute(() -> {
                try {
                    JSONObject snapshot = new JSONObject(RobinAudio.nativeStart(getFilesDir().getAbsolutePath() + "/robin", bufferMs, backgroundBufferMs, automatic));
                    RobinAudio.nativeBackground(background);
                    if (snapshot.optString("state").equals("error")) throw new IllegalStateException(snapshot.optString("error"));
                    if (!destroyed) advertise(snapshot.getInt("port"));
                } catch (Exception error) { RobinAudio.serviceError = error.toString(); main.post(this::stopSelf); }
            });
            worker.scheduleWithFixedDelay(this::updatePlayback, 500, 500, TimeUnit.MILLISECONDS);
        }
        if ("connect".equals(action)) {
            String address = intent.getStringExtra("address");
            String fingerprint = intent.getStringExtra("fingerprint");
            worker.execute(() -> RobinAudio.nativeConnect(address, fingerprint == null ? "" : fingerprint));
            userPaused = false;
        }
        if ("pause".equals(action)) pause();
        if ("play".equals(action)) play();
        return START_NOT_STICKY;
    }

    private void updatePlayback() {
        if (destroyed) return;
        try {
            JSONObject snapshot = new JSONObject(RobinAudio.nativeSnapshot(getFilesDir().getAbsolutePath() + "/robin"));
            String state = snapshot.optString("state");
            if (state.equals("error")) {
                RobinAudio.serviceError = snapshot.optString("error");
                main.post(this::stopSelf);
                return;
            }
            String peer = snapshot.optString("peerName");
            boolean connected = state.equals("playing") || state.equals("buffering") || state.equals("paused");
            boolean playing = state.equals("playing");
            main.post(() -> {
                if (destroyed) return;
                if (connected && !userPaused && !focusRequested) play();
                if (!connected && focusRequested) {
                    audio.abandonAudioFocusRequest(focus);
                    focusRequested = false;
                    hasFocus = false;
                    RobinAudio.nativePause(true);
                }
                String title = connected ? peer : "等待电脑连接";
                String key = state + title;
                if (!key.equals(lastNotification)) {
                    lastNotification = key;
                    media.setMetadata(new MediaMetadata.Builder().putString(MediaMetadata.METADATA_KEY_TITLE, title)
                        .putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, artwork)
                        .putBitmap(MediaMetadata.METADATA_KEY_DISPLAY_ICON, artwork)
                        .putString(MediaMetadata.METADATA_KEY_ARTIST, "知更鸟 · Robin").build());
                    media.setPlaybackState(new PlaybackState.Builder()
                        .setActions(PlaybackState.ACTION_PLAY | PlaybackState.ACTION_PAUSE | PlaybackState.ACTION_STOP)
                        .setState(playing ? PlaybackState.STATE_PLAYING : connected ? PlaybackState.STATE_PAUSED : PlaybackState.STATE_NONE,
                            PlaybackState.PLAYBACK_POSITION_UNKNOWN, playing ? 1 : 0).build());
                    getSystemService(NotificationManager.class).notify(4211, notification(title, playing));
                }
            });
        } catch (Exception ignored) { /* 生命周期切换期间下一次刷新会恢复。 */ }
    }

    private PendingIntent command(String action, int id) {
        return PendingIntent.getService(this, id, new Intent(this, RobinAudioService.class).setAction(action),
            PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    private Notification notification(String title, boolean playing) {
        Intent launch = getPackageManager().getLaunchIntentForPackage(getPackageName());
        PendingIntent content = PendingIntent.getActivity(this, 0, launch, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        return new Notification.Builder(this, "robin-audio").setContentTitle(title).setContentText("知更鸟 · Robin")
            .setSmallIcon(android.R.drawable.ic_media_play).setLargeIcon(artwork).setOngoing(true).setContentIntent(content)
            .setCategory(Notification.CATEGORY_TRANSPORT).setVisibility(Notification.VISIBILITY_PUBLIC)
            .addAction(new Notification.Action.Builder(playing ? android.R.drawable.ic_media_pause : android.R.drawable.ic_media_play,
                playing ? "暂停" : "继续播放", command(playing ? "pause" : "play", 1)).build())
            .addAction(new Notification.Action.Builder(android.R.drawable.ic_menu_close_clear_cancel, "关闭接收", command("stop", 2)).build())
            .setStyle(new Notification.MediaStyle().setMediaSession(media.getSessionToken()).setShowActionsInCompactView(0, 1)).build();
    }

    private synchronized void advertise(int port) {
        if (destroyed) return;
        String id = getSharedPreferences("robin", MODE_PRIVATE).getString("deviceId", null);
        if (id == null) { id = UUID.randomUUID().toString(); getSharedPreferences("robin", MODE_PRIVATE).edit().putString("deviceId", id).apply(); }
        NsdServiceInfo service = new NsdServiceInfo();
        service.setServiceName("Robin-" + id.substring(0, 8));
        service.setServiceType("_robin._tcp.");
        service.setPort(port);
        service.setAttribute("protocol", "2");
        service.setAttribute("name", Build.MODEL);
        service.setAttribute("id", id);
        nsd = getSystemService(NsdManager.class);
        registration = new NsdManager.RegistrationListener() {
            @Override public void onServiceRegistered(NsdServiceInfo info) {}
            @Override public void onServiceUnregistered(NsdServiceInfo info) {}
            @Override public void onRegistrationFailed(NsdServiceInfo info, int code) { RobinAudio.serviceError = "设备发现注册失败：" + code; }
            @Override public void onUnregistrationFailed(NsdServiceInfo info, int code) {}
        };
        nsd.registerService(service, NsdManager.PROTOCOL_DNS_SD, registration);
        main.post(this::discoverDesktops);
    }

    private void discoverDesktops() {
        if (destroyed || discovery != null) return;
        discovery = new NsdManager.DiscoveryListener() {
            @Override public void onDiscoveryStarted(String type) {}
            @Override public void onDiscoveryStopped(String type) {}
            @Override public void onStartDiscoveryFailed(String type, int code) {
                main.post(() -> {
                    if (destroyed) return;
                    discovery = null;
                    main.postDelayed(RobinAudioService.this::discoverDesktops, 5000);
                });
            }
            @Override public void onStopDiscoveryFailed(String type, int code) {}
            @Override public void onServiceFound(NsdServiceInfo info) {
                main.post(() -> {
                    if (destroyed) return;
                    discoveredServices.add(info.getServiceName());
                    resolveQueue.add(info);
                    resolveNextDesktop();
                });
            }
            @Override public void onServiceLost(NsdServiceInfo info) {
                main.post(() -> {
                    if (destroyed) return;
                    discoveredServices.remove(info.getServiceName());
                    resolveQueue.removeIf(queued -> queued.getServiceName().equals(info.getServiceName()));
                    RobinAudio.nativeDiscoveredDesktop(info.getServiceName(), "", "");
                });
            }
        };
        nsd.discoverServices("_robin-desktop._tcp.", NsdManager.PROTOCOL_DNS_SD, discovery);
    }

    @SuppressWarnings("deprecation")
    private void resolveNextDesktop() {
        if (destroyed || resolving || resolveQueue.isEmpty()) return;
        NsdServiceInfo candidate = resolveQueue.remove();
        resolving = true;
        // 旧版 Android 同时只能解析一个服务，按队列处理并丢弃已经消失的结果。
        nsd.resolveService(candidate, new NsdManager.ResolveListener() {
            @Override public void onResolveFailed(NsdServiceInfo info, int code) {
                main.post(() -> {
                    resolving = false;
                    if (!destroyed && discoveredServices.contains(candidate.getServiceName())) {
                        main.postDelayed(() -> {
                            if (destroyed || !discoveredServices.contains(candidate.getServiceName())) return;
                            resolveQueue.add(candidate);
                            resolveNextDesktop();
                        }, 5000);
                    }
                    resolveNextDesktop();
                });
            }
            @Override public void onServiceResolved(NsdServiceInfo info) {
                main.post(() -> {
                    resolving = false;
                    if (destroyed) return;
                    if (discoveredServices.contains(candidate.getServiceName()) && info.getHost() instanceof Inet4Address) {
                        byte[] protocol = info.getAttributes().get("protocol");
                        byte[] fingerprint = info.getAttributes().get("fingerprint");
                        if (protocol != null && fingerprint != null && "2".equals(new String(protocol, StandardCharsets.UTF_8))) {
                            RobinAudio.nativeDiscoveredDesktop(candidate.getServiceName(), new String(fingerprint, StandardCharsets.UTF_8),
                                info.getHost().getHostAddress() + ":" + info.getPort());
                        }
                    }
                    // 同名服务的地址也可能变化，定期刷新而不依赖再次出现 onServiceFound。
                    main.postDelayed(() -> {
                        if (destroyed || !discoveredServices.contains(candidate.getServiceName())) return;
                        if (resolveQueue.stream().noneMatch(queued -> queued.getServiceName().equals(candidate.getServiceName()))) {
                            resolveQueue.add(candidate);
                            resolveNextDesktop();
                        }
                    }, 15000);
                    resolveNextDesktop();
                });
            }
        });
    }

    @Override public synchronized void onDestroy() {
        destroyed = true;
        main.removeCallbacksAndMessages(null);
        getApplication().unregisterActivityLifecycleCallbacks(activityLifecycle);
        if (nsd != null && registration != null) { try { nsd.unregisterService(registration); } catch (IllegalArgumentException ignored) {} }
        if (nsd != null && discovery != null) { try { nsd.stopServiceDiscovery(discovery); } catch (IllegalArgumentException ignored) {} }
        worker.execute(RobinAudio::nativeStop);
        worker.shutdown();
        if (audio != null && focus != null) audio.abandonAudioFocusRequest(focus);
        if (media != null) { media.setActive(false); media.release(); }
        if (wifiLock != null && wifiLock.isHeld()) wifiLock.release();
        if (backgroundWifiLock != null && backgroundWifiLock.isHeld()) backgroundWifiLock.release();
        if (wakeLock != null && wakeLock.isHeld()) wakeLock.release();
        super.onDestroy();
    }
    @Override public IBinder onBind(Intent intent) { return null; }
}
