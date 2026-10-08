/**
 * 诊断：验证「限额台账账号列」能把 accountId 解析为用户名（而非 ID 截断）。
 * 用法（需先编译一次 account-display）：
 *   ./node_modules/.bin/tsc src/lib/account-display.ts --outDir dist-test --module esnext --target es2020
 *   node scripts/verify-ledger-account-name.mjs
 * 背景：台账事件里的 accountId 是账号 id，而早期实现按 uid 匹配 → 永远匹配不上。
 */
/**
 * 用真实数据复现「限额台账账号列显示 ID」问题，并验证修复后的匹配逻辑。
 * - 旧逻辑：accounts.find(a => a.uid === accountId) → 回退 accountId.slice(0,8)（= 26a3045b）
 * - 新逻辑：id → uid → 前缀 匹配，命中后走 displayName()（真实引入的上游实现）
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { displayName } from "../dist-test/account-display.js";

const store = path.join(os.homedir(), ".wb-switch");
const accounts = (() => {
  const raw = JSON.parse(fs.readFileSync(path.join(store, "accounts.json"), "utf8"));
  return Array.isArray(raw) ? raw : raw.accounts ?? [];
})();
const ledger = JSON.parse(fs.readFileSync(path.join(store, "rate_limit_state.json"), "utf8"));
const events = ledger.events ?? [];

const oldLogic = (accountId) => {
  const match = accounts.find((a) => a.uid === accountId);
  return match?.nickname || match?.email || String(accountId).slice(0, 8);
};
const newLogic = (accountId) => {
  const key = String(accountId);
  const match =
    accounts.find((a) => a.id === key) ??
    accounts.find((a) => a.uid === key) ??
    accounts.find((a) => a.id.startsWith(key) || (a.uid ?? "").startsWith(key));
  return match ? displayName(match) : key.slice(0, 8);
};

console.log("=== 账号库（id / uid / nickname 对照，脱敏）===");
for (const a of accounts.slice(0, 4)) {
  console.log(
    `  id=${String(a.id).slice(0, 8)}  uid=${String(a.uid).slice(0, 8)}  nickname=${a.nickname}`,
  );
}

console.log("\n=== 台账事件：旧逻辑 vs 新逻辑 ===");
let fixed = 0;
for (const e of events) {
  const before = oldLogic(e.accountId);
  const after = newLogic(e.accountId);
  const ok = before !== after && after !== String(e.accountId).slice(0, 8);
  if (ok) fixed += 1;
  console.log(
    `  accountId=${String(e.accountId).slice(0, 8)}  旧: ${String(before).padEnd(14)} 新: ${String(after).padEnd(16)} ${ok ? "✅ 已修正" : "（无变化）"}`,
  );
}
console.log(`\n结论：${fixed}/${events.length} 条事件的账号名由「ID 截断」修正为「用户名」`);
