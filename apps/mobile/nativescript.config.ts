import type { NativeScriptConfig } from "@nativescript/core";

export default {
  id: "dev.robin.mobile",
  appPath: "src",
  appResourcesPath: "App_Resources",
  bundler: "vite",
  bundlerConfigPath: "vite.config.mts",
  android: { v8Flags: "--expose_gc", markingMode: "none" },
} satisfies NativeScriptConfig;
