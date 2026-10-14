// Web build of the desktop UI for the headless server. Run from the repo root:
//   pnpm exec vite build --config crates/apw-headless/web/vite.config.ts
// Reuses the desktop config unchanged and only swaps the Tauri modules for
// tauri-shim.ts, plus the query-route panel for NetworkSettings.tsx (the
// server rotates exit proxies instead of driving Clash; it also renders the
// headless Telegram panel). Other upstream UI changes need no edits here.
import path from "node:path";
import { defineConfig, mergeConfig, type ConfigEnv } from "vite";
import desktopConfig from "../../../vite.config";

const shim = path.resolve(__dirname, "tauri-shim.ts");
const networkPanel = path.resolve(__dirname, "NetworkSettings.tsx");

export default defineConfig(async (env: ConfigEnv) =>
  mergeConfig(await desktopConfig(env), {
    resolve: {
      // An array, not an object: mergeConfig puts an array's entries ahead
      // of the desktop config's "@" alias, which would otherwise match first.
      alias: [
        { find: "@tauri-apps/api/core", replacement: shim },
        { find: "@tauri-apps/api/event", replacement: shim },
        { find: "@tauri-apps/plugin-opener", replacement: shim },
        { find: "@/components/NetworkSettings", replacement: networkPanel },
      ],
    },
    build: {
      outDir: path.resolve(__dirname, "dist"),
      emptyOutDir: true,
    },
  }),
);
