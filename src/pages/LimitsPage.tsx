import { useEffect, useMemo, useState } from "react";
import {
  CircleAlert,
  Gauge,
  History,
  Loader2,
  RefreshCw,
  Timer,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import * as api from "@/lib/api";
import type { AccountMeta, LimitEvent, LimitsLedger } from "@/lib/types";

type RangeKey = "7d" | "30d" | "total";

const RANGE_OPTIONS: { key: RangeKey; label: string; days?: number }[] = [
  { key: "7d", label: "近 7 天", days: 7 },
  { key: "30d", label: "近 30 天", days: 30 },
  { key: "total", label: "全部" },
];

function formatDateTime(timestamp?: number | null): string {
  if (!timestamp) return "—";
  return new Date(timestamp).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatDuration(ms: number): string {
  if (ms <= 0) return "已重置";
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1000);
  if (minutes >= 60) {
    const hours = Math.floor(minutes / 60);
    return `${hours} 小时 ${minutes % 60} 分`;
  }
  if (minutes > 0) return `${minutes} 分 ${seconds} 秒`;
  return `${seconds} 秒`;
}

function accountName(uid: string | null | undefined, accounts: AccountMeta[]): string {
  if (!uid) return "—";
  const match = accounts.find((account) => account.uid === uid);
  return match?.nickname || match?.email || uid.slice(0, 8);
}

function LimitRow({
  event,
  accounts,
  now,
}: {
  event: LimitEvent;
  accounts: AccountMeta[];
  now: number;
}) {
  const remaining = event.resetAt - now;
  const active = event.active && remaining > 0;
  return (
    <tr className="border-b border-border/50 last:border-0">
      <td className="px-3 py-2.5 text-xs tabular-nums text-muted-foreground">
        {formatDateTime(event.occurredAt)}
      </td>
      <td className="px-3 py-2.5 text-xs font-medium truncate" title={accountName(event.accountUid, accounts)}>
        {accountName(event.accountUid, accounts)}
      </td>
      <td className="px-3 py-2.5 text-xs truncate" title={event.model ?? "未知模型"}>
        {event.model || "未知模型"}
      </td>
      <td className="px-3 py-2.5 text-xs tabular-nums text-muted-foreground">
        {formatDateTime(event.resetAt)}
      </td>
      <td className="px-3 py-2.5 text-xs">
        {active ? (
          <span className="inline-flex items-center gap-1.5 rounded-full bg-amber-500/15 px-2 py-0.5 font-medium text-amber-600 dark:text-amber-400">
            <Timer className="size-3" aria-hidden="true" />
            {formatDuration(remaining)}
          </span>
        ) : (
          <span className="text-muted-foreground">已重置</span>
        )}
      </td>
    </tr>
  );
}

function LimitsLoadingSkeleton() {
  return (
    <div className="space-y-6" role="status" aria-label="正在扫描本地限额日志…">
      <span className="sr-only">正在扫描本地限额日志…</span>
      <Skeleton className="h-20 w-full rounded-xl" />
      <Skeleton className="h-64 w-full rounded-xl" />
    </div>
  );
}

export default function LimitsPage() {
  const [ledger, setLedger] = useState<LimitsLedger | null>(null);
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [range, setRange] = useState<RangeKey>("7d");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    setError(null);
    const days = RANGE_OPTIONS.find((option) => option.key === range)?.days;
    Promise.all([api.getLimits(days), api.getAccounts().catch(() => ({ accounts: [] as AccountMeta[] }))])
      .then(([result, accountResult]) => {
        if (disposed) return;
        setLedger(result);
        setAccounts(accountResult.accounts);
      })
      .catch((cause) => {
        if (!disposed) setError(api.asError(cause));
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [reload, range]);

  // 限额中的倒计时每秒刷新。
  useEffect(() => {
    if (!ledger?.activeCount) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [ledger?.activeCount]);

  const events = useMemo(() => {
    const list = ledger?.events ?? [];
    return [...list].sort((left, right) => right.occurredAt - left.occurredAt);
  }, [ledger]);

  const activeEvents = useMemo(
    () => events.filter((event) => event.active && event.resetAt - now > 0),
    [events, now],
  );

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-8 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-10">
        <div className="min-w-0">
          {loading && !ledger ? (
            <div aria-hidden="true">
              <Skeleton className="h-8 w-40" />
              <Skeleton className="mt-2 h-5 w-72 max-w-full" />
            </div>
          ) : (
            <>
              <h1 className="text-[28px] font-semibold tracking-tight">限额台账</h1>
              <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
                扫描本地 WorkBuddy 日志记录的 429 频率限制与官方重置时间，数据更新于{" "}
                {ledger ? formatDateTime(ledger.generatedAt) : "—"}
              </p>
            </>
          )}
        </div>
        <div className="flex max-w-full flex-wrap items-center justify-end gap-2">
          {loading && !ledger ? (
            <Skeleton className="h-8 w-44 rounded-lg" aria-hidden="true" />
          ) : (
            <Tabs
              className="min-w-0 gap-0"
              value={range}
              onValueChange={(value) => setRange(value as RangeKey)}
            >
              <TabsList className="h-auto w-fit" aria-label="扫描范围">
                {RANGE_OPTIONS.map((option) => (
                  <TabsTrigger key={option.key} value={option.key} className="px-3">
                    {option.label}
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
          )}
          <Button
            className="shrink-0"
            variant="outline"
            size="sm"
            onClick={() => {
              setNow(Date.now());
              setReload((value) => value + 1);
            }}
            disabled={loading}
          >
            {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            刷新
          </Button>
        </div>
      </header>

      {error && (
        <Alert variant="destructive" className="mb-5">
          <CircleAlert />
          <AlertTitle>台账加载失败</AlertTitle>
          <AlertDescription className="flex flex-wrap items-center gap-3">
            <span>{error}</span>
            <Button size="sm" variant="outline" onClick={() => setReload((value) => value + 1)}>
              重试
            </Button>
          </AlertDescription>
        </Alert>
      )}

      {loading && !ledger ? (
        <LimitsLoadingSkeleton />
      ) : ledger ? (
        <div className="space-y-8">
          <section className="grid grid-cols-1 gap-4 sm:grid-cols-3" aria-labelledby="limits-summary-title">
            <h2 id="limits-summary-title" className="sr-only">
              限额概览
            </h2>
            <Card className="rounded-2xl bg-card/70 py-0 shadow-none">
              <CardContent className="flex items-center gap-3 px-5 py-4">
                <div className="flex size-10 items-center justify-center rounded-full bg-amber-500/15 text-amber-600 dark:text-amber-400">
                  <Timer className="size-5" aria-hidden="true" />
                </div>
                <div>
                  <div className="text-xs text-muted-foreground">当前仍在限额中</div>
                  <div className="text-2xl font-semibold tabular-nums">{activeEvents.length}</div>
                </div>
              </CardContent>
            </Card>
            <Card className="rounded-2xl bg-card/70 py-0 shadow-none">
              <CardContent className="flex items-center gap-3 px-5 py-4">
                <div className="flex size-10 items-center justify-center rounded-full bg-primary/10 text-primary">
                  <History className="size-5" aria-hidden="true" />
                </div>
                <div>
                  <div className="text-xs text-muted-foreground">所选范围记录总数</div>
                  <div className="text-2xl font-semibold tabular-nums">{events.length}</div>
                </div>
              </CardContent>
            </Card>
            <Card className="rounded-2xl bg-card/70 py-0 shadow-none">
              <CardContent className="flex items-center gap-3 px-5 py-4">
                <div className="flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
                  <Gauge className="size-5" aria-hidden="true" />
                </div>
                <div>
                  <div className="text-xs text-muted-foreground">扫描日志文件数</div>
                  <div className="text-2xl font-semibold tabular-nums">{ledger.filesScanned}</div>
                </div>
              </CardContent>
            </Card>
          </section>

          {activeEvents.length > 0 && (
            <section aria-labelledby="limits-active-title">
              <h2 id="limits-active-title" className="mb-2.5 px-1 text-[13px] font-medium leading-5">
                限额中
              </h2>
              <Card className="rounded-2xl border-amber-500/30 bg-amber-500/[0.04] py-0 shadow-none">
                <CardContent className="p-0">
                  <div className="overflow-x-auto">
                    <table className="w-full min-w-[640px] border-collapse">
                      <thead>
                        <tr className="border-b border-amber-500/20 text-left text-[11px] uppercase text-muted-foreground">
                          <th className="px-3 py-2 font-medium">触发时间</th>
                          <th className="px-3 py-2 font-medium">账号</th>
                          <th className="px-3 py-2 font-medium">模型</th>
                          <th className="px-3 py-2 font-medium">预计重置</th>
                          <th className="px-3 py-2 font-medium">倒计时</th>
                        </tr>
                      </thead>
                      <tbody>
                        {activeEvents.map((event, index) => (
                          <LimitRow key={`${event.sessionId ?? "x"}-${event.resetAt}-${index}`} event={event} accounts={accounts} now={now} />
                        ))}
                      </tbody>
                    </table>
                  </div>
                </CardContent>
              </Card>
            </section>
          )}

          <section aria-labelledby="limits-history-title">
            <h2 id="limits-history-title" className="mb-2.5 px-1 text-[13px] font-medium leading-5">
              限额历史
            </h2>
            <Card className="min-w-0 gap-0 rounded-xl py-0 shadow-none">
              <CardHeader className="px-4 pt-3 pb-0 sm:px-5">
                <CardDescription className="text-xs">
                  按发生时间倒序排列；账号与模型来自会话反查，仅供参考。
                </CardDescription>
              </CardHeader>
              <CardContent className="px-0 pt-2 pb-2 sm:px-0">
                {events.length === 0 ? (
                  <div className="rounded-lg border border-dashed px-4 py-10 text-center text-sm text-muted-foreground">
                    所选范围内暂无 429 限额记录。
                  </div>
                ) : (
                  <div className="overflow-x-auto">
                    <table className="w-full min-w-[640px] border-collapse">
                      <thead>
                        <tr className="border-b border-border/60 text-left text-[11px] uppercase text-muted-foreground">
                          <th className="px-3 py-2 font-medium">触发时间</th>
                          <th className="px-3 py-2 font-medium">账号</th>
                          <th className="px-3 py-2 font-medium">模型</th>
                          <th className="px-3 py-2 font-medium">重置时间</th>
                          <th className="px-3 py-2 font-medium">状态</th>
                        </tr>
                      </thead>
                      <tbody>
                        {events.map((event, index) => (
                          <LimitRow key={`${event.sessionId ?? "x"}-${event.resetAt}-${index}`} event={event} accounts={accounts} now={now} />
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </CardContent>
            </Card>
          </section>

          {ledger.parseErrors > 0 && (
            <p className="flex items-center gap-1.5 px-1 text-xs text-amber-600">
              <CircleAlert className="size-3.5" aria-hidden="true" />
              已跳过 {ledger.parseErrors} 条无法解析的本地日志记录。
            </p>
          )}
        </div>
      ) : !error ? (
        <div className="rounded-xl border border-dashed px-4 py-16 text-center text-sm text-muted-foreground">
          暂无限额数据，请点击刷新重试。
        </div>
      ) : null}
    </div>
  );
}
