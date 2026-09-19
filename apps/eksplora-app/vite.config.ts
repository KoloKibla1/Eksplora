import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri dev server on 1420, strictPort to match tauri.conf.json
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "chrome105",
  },
});
