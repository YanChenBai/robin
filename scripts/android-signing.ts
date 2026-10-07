import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

interface AndroidSigning {
  path: string;
  alias: string;
  password: string;
}

export function androidSigning(): AndroidSigning {
  const directory = join(process.env.LOCALAPPDATA ?? homedir(), "Robin", "signing");
  const config = join(directory, "android.json");
  const path = join(directory, "android.p12");
  mkdirSync(directory, { recursive: true });

  if (existsSync(config)) {
    const signing = JSON.parse(readFileSync(config, "utf8")) as AndroidSigning;
    if (!signing.path || !signing.alias || !signing.password || !existsSync(signing.path)) {
      throw new Error("本机 Android 签名配置或签名文件不完整，请恢复 Robin/signing 中的备份。");
    }
    return signing;
  }
  if (existsSync(path)) {
    throw new Error("Android 签名文件已存在但缺少配置，请恢复 android.json，避免更换已有签名。");
  }

  const signing = { path, alias: "robin", password: randomBytes(32).toString("hex") };
  const keytool = process.env.JAVA_HOME
    ? join(process.env.JAVA_HOME, "bin", process.platform === "win32" ? "keytool.exe" : "keytool")
    : "keytool";
  const result = spawnSync(
    keytool,
    [
      "-genkeypair",
      "-storetype",
      "PKCS12",
      "-keystore",
      path,
      "-alias",
      signing.alias,
      "-keyalg",
      "RSA",
      "-keysize",
      "3072",
      "-validity",
      "10000",
      "-dname",
      "CN=Robin, O=Robin",
      "-storepass:env",
      "ROBIN_KEYSTORE_PASSWORD",
      "-keypass:env",
      "ROBIN_KEYSTORE_PASSWORD",
      "-noprompt",
    ],
    {
      stdio: "inherit",
      env: { ...process.env, ROBIN_KEYSTORE_PASSWORD: signing.password },
    },
  );
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error("无法生成 Android 正式版签名文件。");
  writeFileSync(config, JSON.stringify(signing, null, 2), { flag: "wx", mode: 0o600 });
  console.log(`已生成本机 Android 签名，后续构建复用：${path}`);
  return signing;
}
