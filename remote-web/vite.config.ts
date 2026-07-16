import { defineConfig } from "vite";
import preact from "@preact/preset-vite";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [preact(), tailwindcss()],
  // 构建产物给 gateway / smeltd 静态托管
  base: "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
  },
  server: {
    port: 5173,
    proxy: {
      // 开发时把 API / WS 转到本机测试网关
      "/sessions": "http://127.0.0.1:18765",
      "/s": {
        target: "http://127.0.0.1:18765",
        ws: true,
      },
    },
  },
});
