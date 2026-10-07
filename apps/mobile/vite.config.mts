// NativeScript CLI requires the standard Vite package's executable.
// eslint-disable-next-line vite-plus/prefer-vite-plus-imports
import { defineConfig, mergeConfig } from "vite";

export default defineConfig(async ({ mode }) => {
  // Vite+ reads every workspace config from the root to discover tasks.
  // NativeScript's helper reads app metadata from cwd, so load it only in its CLI.
  if (!process.env.ROBIN_NATIVE_BUILD) return {};
  const { vueConfig } = await import("@nativescript/vite/vue");
  const release = process.env.ROBIN_RELEASE_BUILD === "1";
  return mergeConfig(vueConfig({ mode: release ? "production" : mode }), {
    resolve: { preserveSymlinks: false },
    ...(release ? { build: { sourcemap: false, minify: true } } : {}),
  });
});
