import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import postcss from "postcss";
import path from "node:path";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;
// GitHub Pages serves only the public demo from the repository subpath.
// Normal WebUI and Tauri builds intentionally keep Vite's root base.
// @ts-expect-error process is a nodejs global
const base = process.env.VITE_PAGES_DEMO === "1" ? "/workbuddy-switch/" : "/";

/**
 * 旧 WebKit（Safari < 15.4）不认识 @layer，会把 Tailwind 的 theme/base/utilities
 * 整块丢弃。生产构建额外输出一份把 @layer 扁平化的 legacy.css；index.html 通过
 * 能力检测只在旧内核加载它，现代浏览器继续使用原始的分层 CSS。
 */
function legacyCssPlugin() {
  return {
    name: "wb-legacy-css",
    apply: "build" as const,
    generateBundle(
      _options: unknown,
      bundle: Record<string, { type: string; fileName: string; source?: string | Uint8Array }>,
    ) {
      // 保持 Vite bundle 的插入顺序（index.css 在页面 chunk CSS 之前），
      // 不要按文件名排序：排序会改变旧内核的级联顺序。
      const cssFiles = Object.values(bundle).filter(
        (item) => item.type === "asset" && item.fileName.endsWith(".css"),
      );
      if (cssFiles.length === 0) return;
      const flattened = cssFiles
        .map((asset) => {
          const source =
            typeof asset.source === "string"
              ? asset.source
              : new TextDecoder().decode(asset.source as Uint8Array);
          const root = postcss.parse(source);
          root.walkAtRules("layer", (atRule) => {
            atRule.replaceWith(atRule.nodes ?? []);
          });
          return root.toString();
        })
        .join("\n");
      this.emitFile({ type: "asset", fileName: "legacy.css", source: flattened });
    },
  };
}

// https://vite.dev/config/
export default defineConfig(async () => ({
  base,
  plugins: [react(), tailwindcss(), legacyCssPlugin()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },

  // 注意：不要在这里加 build.rollupOptions.output.manualChunks。
  // 2026-11-22 用过一次（拆 react/radix/vendor，主包 658KB→212KB）：产物照样构建成功，
  // 但拆出的 radix chunk 只有 1,101 B（空壳），入口模块求值失败，桌面端直接白屏
  // 并显示「应用启动失败」，而当时所有门禁都是绿的。
  // 若确实要拆，先确认 scripts/smoke-webui.py 能在真实浏览器里挂载成功再提交。

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
