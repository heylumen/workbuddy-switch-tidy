import type {
  TokenStatsAccount,
  TokenStatsGroup,
  TokenStatsSource,
  TokenStatsTotals,
} from "@/lib/types";

export const ACCOUNT_UNKNOWN_KEY = "unattributed";

export function tokenTotal(value: TokenStatsTotals): number {
  return value.input + value.output + value.cacheWrite;
}

/** 总账减已归属账号；负数按 0 处理，避免浮点/历史数据造成负条。 */
export function subtractTotals(
  total: TokenStatsTotals,
  known: TokenStatsTotals,
): TokenStatsTotals {
  const input = Math.max(0, total.input - known.input);
  const output = Math.max(0, total.output - known.output);
  const cacheWrite = Math.max(0, total.cacheWrite - known.cacheWrite);
  const cacheRead = Math.max(0, total.cacheRead - known.cacheRead);
  return {
    total: input + output + cacheWrite,
    input,
    output,
    cacheRead,
    cacheWrite,
    uncachedInput: Math.max(0, input - cacheRead),
    records: Math.max(0, total.records - known.records),
    cacheHitRate: input > 0 ? cacheRead / input : null,
  };
}

function zeroTotals(): TokenStatsTotals {
  return {
    total: 0,
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    uncachedInput: 0,
    records: 0,
    cacheHitRate: null,
  };
}

/** 未归属 = 总账 - 所有已归属账号；没有剩余时返回 null。 */
export function unattributedAccount(
  source: TokenStatsSource,
  known: TokenStatsAccount[],
): TokenStatsAccount | null {
  const knownTotal = known.reduce(
    (sum, account) => ({
      total: sum.total + account.summary.total,
      input: sum.input + account.summary.input,
      output: sum.output + account.summary.output,
      cacheRead: sum.cacheRead + account.summary.cacheRead,
      cacheWrite: sum.cacheWrite + account.summary.cacheWrite,
      uncachedInput: sum.uncachedInput + account.summary.uncachedInput,
      records: sum.records + account.summary.records,
      cacheHitRate: null,
    }),
    zeroTotals(),
  );
  const summary = subtractTotals(source.summary, knownTotal);
  if (summary.records === 0 && summary.total === 0) return null;

  const knownDaily = new Map<string, TokenStatsTotals>();
  known.forEach((account) => {
    account.daily.forEach((point) => {
      const current = knownDaily.get(point.key);
      knownDaily.set(
        point.key,
        current
          ? {
              total: current.total + point.total,
              input: current.input + point.input,
              output: current.output + point.output,
              cacheRead: current.cacheRead + point.cacheRead,
              cacheWrite: current.cacheWrite + point.cacheWrite,
              uncachedInput: current.uncachedInput + point.uncachedInput,
              records: current.records + point.records,
              cacheHitRate: null,
            }
          : point,
      );
    });
  });

  const daily = source.daily
    .map((point) => {
      const knownPoint = knownDaily.get(point.key);
      return {
        ...point,
        ...(knownPoint ? subtractTotals(point, knownPoint) : point),
        key: point.key,
      } as TokenStatsGroup;
    })
    .filter((point) => point.total > 0 || point.records > 0);

  return {
    key: ACCOUNT_UNKNOWN_KEY,
    summary,
    models: [],
    daily,
  };
}
