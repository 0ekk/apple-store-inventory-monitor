// Web build of the desktop UI for the headless server. Run from the repo root:
//   pnpm exec vite build --config crates/apw-headless/web/vite.config.ts
// Reuses the desktop config unchanged and only swaps the Tauri modules for
// tauri-shim.ts, so upstream UI changes need no edits here.
import path from "node:path";
import { defineConfig, mergeConfig, type ConfigEnv } from "vite";
import desktopConfig from "../../../vite.config";

const shim = path.resolve(__dirname, "tauri-shim.ts");

export default defineConfig(async (env: ConfigEnv) =>
  mergeConfig(await desktopConfig(env), {
    resolve: {
      alias: {
        "@tauri-apps/api/core": shim,
        "@tauri-apps/api/event": shim,
        "@tauri-apps/plugin-opener": shim,
      },
    },
    build: {
      outDir: path.resolve(__dirname, "dist"),
      emptyOutDir: true,
    },
  }),
);
