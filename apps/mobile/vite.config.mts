// NativeScript CLI requires the standard Vite package's executable.
// eslint-disable-next-line vite-plus/prefer-vite-plus-imports
import { defineConfig, mergeConfig } from "vite";

export default defineConfig(async ({ mode }) => {
  // Vite+ reads every workspace config from the root to discover tasks.
  // NativeScript's helper reads app metadata from cwd, so load it only in its CLI.
  if (!process.env.ROBIN_NATIVE_BUILD) return {};
  const { vueConfig } = await import("@nativescript/vite/vue");
  return mergeConfig(vueConfig({ mode }), { resolve: { preserveSymlinks: false } });
});
