import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The dev server binds one address: PROW_DEV_HOST, or loopback. It proxies the API (index, SQL)
// to a prow-server's HTTP side: PROW_HTTP_URL, or one on this host.
const host = process.env.PROW_DEV_HOST ?? "127.0.0.1";
const server = process.env.PROW_HTTP_URL ?? "http://127.0.0.1:7381";
export default defineConfig({
  plugins: [react()],
  server: { host, port: 5181, strictPort: true, proxy: { "/api": server } },
  preview: { host, port: 5181, strictPort: true },
  build: { target: "es2022" },
});
