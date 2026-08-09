import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { borgThemePreload } from "@borg/ui/vite";

export default defineConfig({
  plugins: [react(), tailwindcss(), borgThemePreload()],
  // The gateway serves this SPA under /ui/ (crates/gateway/src/webui.rs).
  base: "/ui/",
  server: {
    port: 5174,
    strictPort: true,
    // Dev-only: talk to a locally running borgd gateway instead of the embedded build.
    proxy: { "/grpc": { target: "http://127.0.0.1:7842", changeOrigin: true } },
  },
  build: { outDir: "dist", emptyOutDir: true },
});
