import type { AccountMeta, CreditExpiry } from "@/lib/types";
import { displayName } from "@/lib/account-display";

export type AccountSortMode = "smart" | "lastUsed" | "created" | "expiresAt" | "name";

export const ACCOUNT_SORT_STORAGE_KEY = "wb-switch.accounts.sort";

export const ACCOUNT_SORT_OPTIONS: Array<{ value: AccountSortMode; label: string }> = [
  { value: "smart", label: "智能排序" },
  { value: "lastUsed", label: "最近使用" },
  { value: "created", label: "最近添加" },
  { value: "expiresAt", label: "即将到期" },
  { value: "name", label: "账号名" },
];

export function isAccountSortMode(value: string | null): value is AccountSortMode {
  return ACCOUNT_SORT_OPTIONS.some((option) => option.value === value);
}

export function readAccountSortMode(): AccountSortMode {
  try {
    const value = localStorage.getItem(ACCOUNT_SORT_STORAGE_KEY);
    return isAccountSortMode(value) ? value : "smart";
  } catch {
    return "smart";
  }
}

export function writeAccountSortMode(mode: AccountSortMode): void {
  try {
    localStorage.setItem(ACCOUNT_SORT_STORAGE_KEY, mode);
  } catch {
    /* 隐私模式：本次会话仍按 state 生效 */
  }
}

export function expiringSoonAmount(credit?: CreditExpiry): number {
  return credit?.ok ? credit.expiringSoonRemaining ?? 0 : 0;
}

export function hasExpiringSoonCredits(credit?: CreditExpiry): boolean {
  return credit?.ok === true && expiringSoonAmount(credit) > 0;
}

function soonestRelevantExpiry(credit?: CreditExpiry): number {
  const soonestExpiringCredit = (credit?.resources ?? [])
    .filter((resource) => resource.remaining > 0 && resource.expiringSoon && resource.expireAt != null)
    .map((resource) => resource.expireAt as number)
    .reduce((soonest, expireAt) => Math.min(soonest, expireAt), Number.POSITIVE_INFINITY);
  return Number.isFinite(soonestExpiringCredit)
    ? soonestExpiringCredit
    : credit?.soonestExpireAt ?? Number.POSITIVE_INFINITY;
}

function creditPriorityRank(credit?: CreditExpiry): number {
  if (!credit?.ok) return 3;
  if (hasExpiringSoonCredits(credit)) return 0;
  if (credit.expired) return 1;
  return 2;
}

function compareNullableAsc(
  left: number | null | undefined,
  right: number | null | undefined,
): number {
  const a = left ?? Number.POSITIVE_INFINITY;
  const b = right ?? Number.POSITIVE_INFINITY;
  if (a === b) return 0;
  return a - b;
}

function compareNullableDesc(
  left: number | null | undefined,
  right: number | null | undefined,
): number {
  const a = left ?? 0;
  const b = right ?? 0;
  if (a === b) return 0;
  if (!a) return 1;
  if (!b) return -1;
  return b - a;
}

/**
 * 账号列表排序。
 *
 * - smart 逐项复刻账号页原有「积分优先级」排序；积分未全部就绪时保持原顺序。
 * - 其余模式只在前端派生，绝不写回 accounts.json 的数组顺序：
 *   CodeBuddy CLI 的 `active` 使用数组下标，写回会让历史下标错位。
 */
export function sortAccounts(
  accounts: AccountMeta[],
  mode: AccountSortMode,
  context: {
    creditMap: Record<string, CreditExpiry>;
    creditOrderingReady: boolean;
  },
): AccountMeta[] {
  const indexed = accounts.map((account, index) => ({ account, index }));

  if (mode === "smart") {
    if (!context.creditOrderingReady) return accounts;
    return indexed
      .sort((left, right) => {
        const leftCredit = context.creditMap[left.account.id];
        const rightCredit = context.creditMap[right.account.id];
        const rankDifference = creditPriorityRank(leftCredit) - creditPriorityRank(rightCredit);
        if (rankDifference !== 0) return rankDifference;

        const leftExpiry = soonestRelevantExpiry(leftCredit);
        const rightExpiry = soonestRelevantExpiry(rightCredit);
        if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry;

        const amountDifference = expiringSoonAmount(rightCredit) - expiringSoonAmount(leftCredit);
        if (amountDifference !== 0) return amountDifference;
        return left.index - right.index;
      })
      .map(({ account }) => account);
  }

  if (mode === "lastUsed") {
    return indexed
      .sort((left, right) => {
        const difference = compareNullableDesc(left.account.lastUsedAt, right.account.lastUsedAt);
        return difference !== 0 ? difference : left.index - right.index;
      })
      .map(({ account }) => account);
  }

  if (mode === "created" || mode === "expiresAt") {
    return indexed
      .sort((left, right) => {
        const leftValue =
          mode === "created" ? left.account.createdAt : left.account.expiresAt;
        const rightValue =
          mode === "created" ? right.account.createdAt : right.account.expiresAt;
        const difference =
          mode === "created"
            ? compareNullableDesc(leftValue, rightValue)
            : compareNullableAsc(leftValue, rightValue);
        return difference !== 0 ? difference : left.index - right.index;
      })
      .map(({ account }) => account);
  }

  return indexed
    .sort((left, right) => {
      const difference = displayName(left.account).localeCompare(displayName(right.account), "zh-Hans-CN");
      return difference !== 0 ? difference : left.index - right.index;
    })
    .map(({ account }) => account);
}
