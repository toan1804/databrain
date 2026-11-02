import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const host = process.env.TAURI_DEV_HOST;

// https://v2.tauri.app/start/frontend/vite/
export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/crates/**", "**/target/**"] },
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari15",
    minify: process.env.TAURI_ENV_DEBUG ? false : "esbuild",
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    // Libraries in chunks of their own (they change less often than the app);
    // rarely opened tabs are loaded on first use (see EditorPane).
    chunkSizeWarningLimit: 800,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes("node_modules")) return undefined;
          if (/[\\/]node_modules[\\/](react|react-dom|scheduler)[\\/]/.test(id)) return "react";
          if (/[\\/]node_modules[\\/](@codemirror|@lezer|style-mod|w3c-keyname|crelt)[\\/]/.test(id)) return "codemirror";
          if (/[\\/]node_modules[\\/]@glideapps[\\/]/.test(id)) return "grid";
          if (/[\\/]node_modules[\\/]sql-formatter[\\/]/.test(id)) return undefined;
          return "vendor";
        },
      },
    },
  },
  test: {
    projects: [
      // Pure logic: no DOM.
      { extends: true, test: { name: "unit", environment: "node", include: ["src/**/*.test.ts"] } },
      // Components: rendered with Testing Library in jsdom.
      { extends: true, test: { name: "dom", environment: "jsdom", include: ["src/**/*.test.tsx"], setupFiles: ["src/test/setup.ts"] } },
    ],
  },
});
