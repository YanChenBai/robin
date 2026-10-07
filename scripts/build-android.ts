import { copyFileSync, existsSync, mkdirSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const sdk =
  process.env.ANDROID_HOME ??
  process.env.ANDROID_SDK_ROOT ??
  join(homedir(), "AppData/Local/Android/Sdk");
const ndkRoot = join(sdk, "ndk");
const versions = existsSync(ndkRoot)
  ? readdirSync(ndkRoot).sort((a, b) => a.localeCompare(b, undefined, { numeric: true }))
  : [];
const latest = versions.at(-1);
const ndk = process.env.ANDROID_NDK_HOME ?? (latest ? join(ndkRoot, latest) : undefined);
if (!ndk)
  throw new Error(
    "Android NDK not found. Set ANDROID_NDK_HOME or install NDK in Android SDK Manager.",
  );
const destination = join(root, "packages/nativescript-robin/native/android/src/main/jniLibs");
const buildEnv = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => !/token|secret|password|api.?key|auth/i.test(key)),
);
const result = spawnSync(
  "cargo",
  [
    "ndk",
    "-t",
    "arm64-v8a",
    "--platform",
    "26",
    "--link-libcxx-shared",
    "-o",
    destination,
    "build",
    "--release",
    "-p",
    "robin-mobile",
  ],
  {
    cwd: root,
    stdio: "inherit",
    env: { ...buildEnv, ANDROID_HOME: sdk, ANDROID_NDK_HOME: ndk },
  },
);
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);
// 保留常规输出位置，供工具和真机排查使用。
const library = join(destination, "arm64-v8a/librobin_mobile.so");
if (!existsSync(library))
  throw new Error("Rust build completed without the Android shared library.");
// Oboe's C++ symbols require the same NDK runtime used at link time.
const runtime = join(
  ndk,
  "toolchains/llvm/prebuilt/windows-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so",
);
if (!existsSync(runtime)) throw new Error("Android NDK C++ runtime not found.");
copyFileSync(runtime, join(destination, "arm64-v8a/libc++_shared.so"));
const readelf = join(ndk, "toolchains/llvm/prebuilt/windows-x86_64/bin/llvm-readelf.exe");
const dependencies = spawnSync(readelf, ["-d", library], { encoding: "utf8", env: buildEnv });
if (dependencies.status !== 0 || !dependencies.stdout.includes("[libc++_shared.so]")) {
  throw new Error("Robin Android library is missing its linked C++ runtime dependency.");
}
mkdirSync(join(root, "target/android"), { recursive: true });
copyFileSync(library, join(root, "target/android/librobin_mobile.so"));
