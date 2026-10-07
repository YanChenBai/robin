import { homedir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

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
const result = spawnSync(process.execPath, [cli, ...process.argv.slice(2)], {
  cwd: project,
  stdio: "inherit",
  env: { ...env, ANDROID_HOME: sdk, ANDROID_SDK_ROOT: sdk, ROBIN_NATIVE_BUILD: "1" },
});
if (result.error) throw result.error;
process.exit(result.status ?? 1);
