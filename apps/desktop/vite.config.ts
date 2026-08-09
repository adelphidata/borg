import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { borgThemePreload } from "@borg/ui/vite";

export default defineConfig({
  plugins: [react(), tailwindcss(), borgThemePreload()],
  // Relative so the packaged app can load over file://.
  base: "./",
  server: { port: 5173, strictPort: true },
  build: { outDir: "dist" },
});
