import { homedir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { androidSigning } from "./android-signing.ts";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const project = join(root, "apps/mobile");
const require = createRequire(join(project, "package.json"));
const manifest = require("nativescript/package.json") as { bin: { ns: string } };
const cli = resolve(dirname(require.resolve("nativescript/package.json")), manifest.bin.ns);
const sdk =
  process.env.ANDROID_HOME ??
  process.env.ANDROID_SDK_ROOT ??
  join(homedir(), "AppData/Local/Android/Sdk");
const env = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => !/token|secret|password|api.?key|auth/i.test(key)),
);
const args = process.argv.slice(2);
const release = args.includes("--release");
if (release) {
  const signing = androidSigning();
  args.push(
    "--key-store-path",
    signing.path,
    "--key-store-alias",
    signing.alias,
    "--key-store-password",
    signing.password,
    "--key-store-alias-password",
    signing.password,
  );
}
const result = spawnSync(process.execPath, [cli, ...args], {
  cwd: project,
  stdio: "inherit",
  env: {
    ...env,
    ANDROID_HOME: sdk,
    ANDROID_SDK_ROOT: sdk,
    ROBIN_NATIVE_BUILD: "1",
    ROBIN_RELEASE_BUILD: release ? "1" : "0",
  },
});
if (result.error) throw result.error;
process.exit(result.status ?? 1);
