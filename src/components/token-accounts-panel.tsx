import { useEffect, useMemo, useState } from "react";
import {
  Bar,
  CartesianGrid,
  ComposedChart,
  Line,
  XAxis,
  YAxis,
} from "recharts";

import {
  ChartContainer,
  ChartTooltip,
  type ChartConfig,
} from "@/components/ui/chart";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { displayName } from "@/lib/account-display";
import {
  ACCOUNT_UNKNOWN_KEY,
  tokenTotal,
  unattributedAccount,
} from "@/lib/token-account-utils";
import type {
  AccountMeta,
  TokenStatsAccount,
  TokenStatsGroup,
  TokenStatsSource,
} from "@/lib/types";

const TOKEN_SERIES = [
  { key: "cacheRead", label: "缓存读取", color: "var(--data-series-emerald)" },
  { key: "uncachedInput", label: "新增输入", color: "var(--data-series-teal)" },
  { key: "output", label: "输出", color: "var(--data-series-violet)" },
  { key: "cacheWrite", label: "缓存写入", color: "var(--data-series-amber)" },
] as const;

const ACCOUNT_COLORS = [
  "var(--data-series-indigo)",
  "var(--data-series-sky)",
  "var(--data-series-emerald)",
  "var(--data-series-violet)",
  "var(--data-series-amber)",
  "var(--data-series-rose)",
  "var(--data-series-teal)",
];

const compact = new Intl.NumberFormat("en-US", {
  notation: "compact",
  compactDisplay: "short",
  maximumFractionDigits: 1,
});
const exact = new Intl.NumberFormat("zh-CN");

function formatToken(value: number): string {
  return compact.format(value).replace(/([kmb])$/i, (unit) => unit.toUpperCase());
}

function accountLabel(key: string, accountByUid: Map<string, AccountMeta>): string {
  if (key === ACCOUNT_UNKNOWN_KEY) return "未归属";
  const account = accountByUid.get(key);
  return account ? displayName(account) : key.slice(0, 8);
}

function aggregateDaily(
  accounts: TokenStatsAccount[],
  selected: string,
): Array<Record<string, number | string>> {
  if (selected !== "all") {
    const account = accounts.find((item) => item.key === selected);
    return (account?.daily ?? []).map((point) => ({
      ...point,
      date: String(point.key),
    })) as Array<Record<string, number | string>>;
  }
  const byDate = new Map<string, Record<string, number | string>>();
  accounts.forEach((account) => {
    account.daily.forEach((point: TokenStatsGroup) => {
      const key = String(point.key);
      const row = byDate.get(key) ?? { date: key, records: 0 };
      row[account.key] = tokenTotal(point);
      row.records = Number(row.records) + point.records;
      byDate.set(key, row);
    });
  });
  return [...byDate.values()].sort((left, right) =>
    String(left.date).localeCompare(String(right.date)),
  );
}

export function TokenAccountsPanel({
  source,
  accounts,
}: {
  source: TokenStatsSource;
  accounts: AccountMeta[];
}) {
  const [selected, setSelected] = useState("all");
  const knownRows = source.accounts ?? [];
  const unknown = unattributedAccount(source, knownRows);
  const rows = unknown ? [...knownRows, unknown] : knownRows;
  const accountByUid = useMemo(
    () => new Map(accounts.map((account) => [account.uid ?? "", account])),
    [accounts],
  );
  const total = tokenTotal(source.summary);

  useEffect(() => {
    setSelected("all");
  }, [source.source]);

  if (rows.length === 0) return null;

  const active = selected !== "all" && rows.some((row) => row.key === selected) ? selected : "all";
  const data = aggregateDaily(rows, active);
  const selectedRow = active === "all" ? null : rows.find((row) => row.key === active);

  const config: ChartConfig =
    active === "all"
      ? Object.fromEntries([
          ...rows.map((row, index) => [
            row.key,
            {
              label: accountLabel(row.key, accountByUid),
              color: row.key === ACCOUNT_UNKNOWN_KEY ? "var(--muted-foreground)" : ACCOUNT_COLORS[index % ACCOUNT_COLORS.length],
            },
          ]),
          ["records", { label: "调用次数", color: "var(--data-series-indigo)" }],
        ])
      : {
          ...Object.fromEntries(TOKEN_SERIES.map((item) => [item.key, { label: item.label, color: item.color }])),
          records: { label: "调用次数", color: "var(--data-series-indigo)" },
        };

  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby="token-accounts-title">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 id="token-accounts-title" className="text-base font-semibold tracking-tight">
            账号 Token 与调用情况
          </h2>
          <p className="mt-1 text-xs text-muted-foreground">
            按账号归因；关联组使用规范账号，未关联副本按文件顺序启发式归属，极少数复制时间相同的副本可能偏差。
          </p>
        </div>
        <Select value={active} onValueChange={setSelected}>
          <SelectTrigger size="sm" className="w-44 bg-background text-xs" aria-label="选择账号">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">全部账号</SelectItem>
            {rows.map((row) => (
              <SelectItem key={row.key} value={row.key}>
                {accountLabel(row.key, accountByUid)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <div className="rounded-xl border bg-card p-4">
        <div className="mb-2 flex items-center justify-between text-xs text-muted-foreground">
          <span>{active === "all" ? "全部账号每日用量" : `${accountLabel(active, accountByUid)} 用量构成`}</span>
          <span>累计 {formatToken(selectedRow ? tokenTotal(selectedRow.summary) : total)} Token</span>
        </div>
        <ChartContainer config={config} className="h-64 w-full">
          <ComposedChart data={data} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
            <CartesianGrid vertical={false} strokeDasharray="3 3" />
            <XAxis dataKey="date" tickLine={false} axisLine={false} minTickGap={24} tickFormatter={(value) => String(value).slice(5).replace("-", "/")} />
            <YAxis yAxisId="tokens" tickLine={false} axisLine={false} width={48} tickFormatter={(value) => formatToken(Number(value))} />
            <YAxis yAxisId="calls" orientation="right" tickLine={false} axisLine={false} width={40} allowDecimals={false} />
            <ChartTooltip />
            {active === "all"
              ? rows.map((row, index) => (
                  <Bar
                    key={row.key}
                    yAxisId="tokens"
                    dataKey={row.key}
                    stackId="account-token"
                    fill={row.key === ACCOUNT_UNKNOWN_KEY ? "var(--muted-foreground)" : ACCOUNT_COLORS[index % ACCOUNT_COLORS.length]}
                    isAnimationActive={false}
                  />
                ))
              : TOKEN_SERIES.map((item) => (
                  <Bar
                    key={item.key}
                    yAxisId="tokens"
                    dataKey={item.key}
                    stackId="token-composition"
                    fill={item.color}
                    isAnimationActive={false}
                  />
                ))}
            <Line
              yAxisId="calls"
              type="monotone"
              dataKey="records"
              stroke="var(--data-series-indigo)"
              strokeWidth={2}
              dot={false}
              isAnimationActive={false}
            />
          </ComposedChart>
        </ChartContainer>
      </div>

      <div className="overflow-hidden rounded-xl border">
        {rows.map((row) => {
          const rowTotal = tokenTotal(row.summary);
          return (
            <button
              key={row.key}
              type="button"
              className="flex w-full items-center justify-between gap-3 border-b px-3 py-2 text-left text-xs last:border-b-0 hover:bg-muted/50"
              onClick={() => setSelected(row.key)}
            >
              <span className="min-w-0 flex-1 truncate font-medium">
                {accountLabel(row.key, accountByUid)}
              </span>
              <span className="tabular-nums text-muted-foreground">{formatToken(rowTotal)}</span>
              <span className="w-16 text-right tabular-nums text-muted-foreground">
                {total > 0 ? `${((rowTotal / total) * 100).toFixed(1)}%` : "—"}
              </span>
              <span className="w-16 text-right tabular-nums text-muted-foreground">
                {exact.format(row.summary.records)} 次
              </span>
            </button>
          );
        })}
      </div>
    </section>
  );
}
