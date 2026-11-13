import { describe, expect, it } from "vitest";

import {
  ACCOUNT_UNKNOWN_KEY,
  subtractTotals,
  tokenTotal,
  unattributedAccount,
} from "@/lib/token-account-utils";
import type { TokenStatsAccount, TokenStatsSource, TokenStatsTotals } from "@/lib/types";

const totals = (input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsTotals => ({
  total: input + output + cacheWrite,
  input,
  output,
  cacheRead,
  cacheWrite,
  uncachedInput: Math.max(0, input - cacheRead),
  records,
  cacheHitRate: input > 0 ? cacheRead / input : null,
});

const source: TokenStatsSource = {
  source: "workbuddy",
  summary: totals(100, 50, 20, 10, 10),
  models: [],
  projects: [],
  sessions: [],
  daily: [{ key: "2026-11-08", ...totals(100, 50, 20, 10, 10) }],
  hours: [],
  filesScanned: 1,
  parseErrors: 0,
};

const account: TokenStatsAccount = {
  key: "uid-a",
  summary: totals(60, 30, 10, 5, 6),
  models: [],
  daily: [{ key: "2026-11-08", ...totals(60, 30, 10, 5, 6) }],
};

describe("token-account-utils", () => {
  it("uses input + output + cacheWrite, not cacheRead", () => {
    expect(tokenTotal(totals(100, 50, 20, 10, 1))).toBe(160);
  });

  it("computes unattributed remainder without double counting", () => {
    const unknown = unattributedAccount(source, [account]);
    expect(unknown?.key).toBe(ACCOUNT_UNKNOWN_KEY);
    expect(unknown?.summary.input).toBe(40);
    expect(unknown?.summary.output).toBe(20);
    expect(unknown?.summary.cacheWrite).toBe(5);
    expect(unknown?.summary.records).toBe(4);
    expect(unknown?.daily[0].total).toBe(65);
  });

  it("returns null when all usage is attributed", () => {
    expect(unattributedAccount(source, [{ ...account, summary: source.summary }])).toBeNull();
  });

  it("never produces negative values", () => {
    const result = subtractTotals(totals(1, 0, 0, 0, 1), totals(5, 5, 5, 5, 5));
    expect(result.input).toBe(0);
    expect(result.output).toBe(0);
    expect(result.records).toBe(0);
    expect(result.total).toBe(0);
  });
});
