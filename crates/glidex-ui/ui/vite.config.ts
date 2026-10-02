import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// GLIDEX_API_URL points the dev proxy at another control plane (the e2e
// suite runs a scratch one).
const apiUrl = process.env.GLIDEX_API_URL ?? "http://localhost:8841";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: apiUrl,
        changeOrigin: true,
        ws: true,
        rewrite: (path) => path.replace(/^\/api/, ""),
      },
    },
  },
});
