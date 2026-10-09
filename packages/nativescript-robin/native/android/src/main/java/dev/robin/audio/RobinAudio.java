package dev.robin.audio;

import android.content.Context;
import android.content.Intent;
import android.os.Build;
import org.json.JSONObject;

public final class RobinAudio {
    private static boolean loaded;
    private static Context application;
    static volatile String serviceError = "";
    private RobinAudio() {}

    private static synchronized void ensureLoaded() {
        if (loaded) return;
        try { System.loadLibrary("robin_mobile"); }
        catch (UnsatisfiedLinkError error) {
            // Keep the original loader failure visible instead of poisoning the class.
            throw new IllegalStateException("无法加载 Robin 音频库：" + error.getMessage(), error);
        }
        loaded = true;
    }

    public static void start(Context context, int bufferMs, int backgroundBufferMs, boolean automatic) {
        application = context.getApplicationContext();
        ensureLoaded();
        if (bufferMs < 5 || bufferMs > 100 || backgroundBufferMs < 5 || backgroundBufferMs > 100) throw new IllegalArgumentException("Buffer must be 5–100 ms");
        serviceError = "";
        Intent intent = new Intent(context, RobinAudioService.class);
        intent.putExtra("bufferMs", bufferMs).putExtra("backgroundBufferMs", backgroundBufferMs).putExtra("automatic", automatic);
        if (Build.VERSION.SDK_INT >= 26) context.startForegroundService(intent);
        else context.startService(intent);
    }
    public static void stop(Context context) { context.stopService(new Intent(context, RobinAudioService.class)); }
    public static String snapshot() {
        ensureLoaded();
        if (serviceError.isEmpty()) return nativeSnapshot(directory());
        try {
            JSONObject current = new JSONObject(nativeSnapshot(directory()));
            if (current.optString("state").equals("stopped")) current.put("state", "error");
            return current.put("error", serviceError).toString();
        }
        catch (Exception ignored) { return "{\"state\":\"error\"}"; }
    }
    public static void initialize(Context context) { application = context.getApplicationContext(); ensureLoaded(); }
    private static String directory() { return application.getFilesDir().getAbsolutePath() + "/robin"; }
    public static void connect(Context context, String address, String fingerprint, int bufferMs, int backgroundBufferMs, boolean automatic) {
        initialize(context);
        serviceError = "";
        Intent intent = new Intent(context, RobinAudioService.class).setAction("connect");
        intent.putExtra("address", address).putExtra("fingerprint", fingerprint).putExtra("bufferMs", bufferMs).putExtra("backgroundBufferMs", backgroundBufferMs).putExtra("automatic", automatic);
        context.startForegroundService(intent);
    }
    public static void pause(Context context, boolean paused) {
        context.startService(new Intent(context, RobinAudioService.class).setAction(paused ? "pause" : "play"));
    }
    public static native void nativeConnect(String address, String fingerprint);
    public static native void nativeBuffer(int bufferMs, int backgroundBufferMs, boolean automatic);
    public static native void nativePause(boolean paused);
    public static native void nativeBackground(boolean background);
    public static native String nativeStart(String directory, int bufferMs, int backgroundBufferMs, boolean automatic);
    public static native String nativeSnapshot(String directory);
    public static native void nativeApprove();
    public static native void nativeReject();
    public static native void nativeDisconnect();
    public static void nativeAutoConnect(boolean enabled) {
        ensureLoaded();
        nativeSetAutoConnect(directory(), enabled);
    }
    private static native void nativeSetAutoConnect(String directory, boolean enabled);
    public static native void nativeDiscoveredDesktop(String service, String fingerprint, String address);
    public static native void nativeStop();
}
