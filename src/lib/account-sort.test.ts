import { describe, expect, it } from "vitest";

import type { AccountMeta, CreditExpiry } from "@/lib/types";
import {
  isAccountSortMode,
  readAccountSortMode,
  sortAccounts,
  writeAccountSortMode,
} from "@/lib/account-sort";

function account(overrides: Partial<AccountMeta> & { id: string }): AccountMeta {
  return {
    uid: null,
    email: null,
    nickname: overrides.id,
    enterpriseName: null,
    expiresAt: null,
    refreshExpiresAt: null,
    refreshedAt: null,
    createdAt: null,
    needsRelogin: false,
    needsReloginReason: null,
    ...overrides,
  };
}

const credits: Record<string, CreditExpiry> = {
  a: { ok: true, expiringSoonRemaining: 10, expiringSoon: true, soonestExpireAt: 100 },
  b: { ok: true, expiringSoonRemaining: 0 },
  c: { ok: false },
};
const ready = { creditMap: credits, creditOrderingReady: true };

describe("account-sort", () => {
  it("smart 保持积分优先级：即将到期优先，再按到期时间", () => {
    const accounts = [account({ id: "c" }), account({ id: "a" }), account({ id: "b" })];
    expect(sortAccounts(accounts, "smart", ready).map((a) => a.id)).toEqual(["a", "b", "c"]);
  });

  it("积分未就绪时 smart 保持原顺序", () => {
    const accounts = [account({ id: "b" }), account({ id: "a" })];
    expect(
      sortAccounts(accounts, "smart", { creditMap: {}, creditOrderingReady: false }).map((a) => a.id),
    ).toEqual(["b", "a"]);
  });

  it("lastUsed 降序，从未使用沉底，时间相同保持原顺序", () => {
    const accounts = [
      account({ id: "never" }),
      account({ id: "old", lastUsedAt: 10 }),
      account({ id: "new", lastUsedAt: 100 }),
      account({ id: "same", lastUsedAt: 10 }),
    ];
    expect(sortAccounts(accounts, "lastUsed", ready).map((a) => a.id)).toEqual([
      "new",
      "old",
      "same",
      "never",
    ]);
  });

  it("created/expiresAt 分别按添加时间降序、到期时间升序", () => {
    const accounts = [
      account({ id: "a", createdAt: 1, expiresAt: 300 }),
      account({ id: "b", createdAt: 2, expiresAt: 100 }),
      account({ id: "c", createdAt: 3, expiresAt: null }),
    ];
    expect(sortAccounts(accounts, "created", ready).map((a) => a.id)).toEqual(["c", "b", "a"]);
    expect(sortAccounts(accounts, "expiresAt", ready).map((a) => a.id)).toEqual(["b", "a", "c"]);
  });

  it("name 按显示名排序", () => {
    const accounts = [
      account({ id: "a", nickname: "张三" }),
      account({ id: "b", nickname: "李四" }),
    ];
    expect(sortAccounts(accounts, "name", ready).map((a) => a.id)).toEqual(["b", "a"]);
  });

  it("排序偏好持久化并拒绝非法值", () => {
    const store = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      value: {
        getItem: (key: string) => store.get(key) ?? null,
        setItem: (key: string, value: string) => store.set(key, String(value)),
      },
      configurable: true,
    });
    expect(readAccountSortMode()).toBe("smart");
    writeAccountSortMode("lastUsed");
    expect(readAccountSortMode()).toBe("lastUsed");
    expect(isAccountSortMode("bogus")).toBe(false);
  });
});
