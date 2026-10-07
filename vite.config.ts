import { defineConfig } from "vite-plus";

export default defineConfig({
  fmt: {
    ignorePatterns: [".agents/**", ".trae/**", "apps/mobile/platforms/**", "apps/mobile/hooks/**"],
  },
  lint: {
    jsPlugins: [
      {
        name: "vite-plus",
        specifier: "vite-plus/oxlint-plugin",
      },
    ],
    options: {
      typeAware: true,
      typeCheck: true,
    },
  },
  run: {
    cache: false,
    tasks: {},
  },
});
