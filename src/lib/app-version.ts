/**
 * 应用版本号的唯一读取入口。
 *
 * 值由 `vite.config.ts` 的 `define` 在构建期注入（来自 package.json）。
 * 单测（vitest）与个别工具链不注入 define，因此这里用 `typeof` 兜底，
 * 避免出现 `__APP_VERSION__ is not defined` 这类与业务无关的测试失败。
 */
export const APP_VERSION: string =
  typeof __APP_VERSION__ === "string" ? __APP_VERSION__ : "0.0.0-dev";
