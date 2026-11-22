import { useCallback, useEffect, useState } from "react";
import { Layers3, Loader2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import type { WbVariant } from "@/lib/types";

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.round(bytes / 1024)} KB`;
}

/**
 * 逻辑会话普查：把「不同账号下的同一段对话」聚成一族展示。
 *
 * 这是 canonical/alias 模型的只读视图：canonical = 逻辑会话（关联组 id），
 * alias = 各账号下的副本会话。用于回答「我到底有多少个重复的同一段对话」。
 */
export function CanonicalAuditDialog({ variant }: { variant: WbVariant }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<api.CanonicalAudit | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  const run = useCallback(async (force = false) => {
    setBusy(true);
    setError(null);
    try {
      const data = await api.auditCanonical(variant, 20, force);
      if (!data.ok) {
        setError(data.reason || "普查失败");
        setResult(null);
        return;
      }
      setResult(data);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }, [variant]);

  useEffect(() => {
    if (open && !result && !busy) void run();
  }, [open, result, busy, run]);

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="shrink-0">
          <Layers3 />逻辑会话
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>逻辑会话普查</DialogTitle>
          <DialogDescription>
            把不同账号下的同一段对话聚成一族（canonical = 逻辑会话，各账号副本 = alias）。
            只读，不改动任何数据。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          {busy && (
            <p className="flex items-center gap-2 rounded-md border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" />
              正在按内容指纹聚类（约 10–60 秒）…
            </p>
          )}
          {error && (
            <p className="rounded-md border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-xs text-destructive">
              {error}
            </p>
          )}

          {result && !busy && (
            <>
              <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                <span>会话 {result.totalSessions ?? 0} 条</span>
                <span>·</span>
                <span>
                  逻辑族 {result.stats?.logicalClusters ?? 0}（已关联 {result.stats?.coveredClusters ?? 0} / 未关联{" "}
                  {result.stats?.uncoveredClusters ?? 0}）
                </span>
                {result.cached ? <Badge variant="outline" className="text-[10px]">缓存结果</Badge> : null}
                {result.budgetExhausted ? <Badge variant="outline" className="text-[10px]">部分仅按体积近似</Badge> : null}
              </div>

              <div className="max-h-[52vh] space-y-1.5 overflow-y-auto">
                {(result.clusters ?? []).map((cluster) => {
                  const key = `${cluster.title}|${cluster.cwd}|${cluster.bytes}`;
                  const isOpen = expanded === key;
                  return (
                    <div key={key} className="rounded-md border">
                      <button
                        type="button"
                        className="flex w-full flex-wrap items-center justify-between gap-2 p-2 text-left"
                        onClick={() => setExpanded(isOpen ? null : key)}
                      >
                        <span className="min-w-0">
                          <span className="block truncate text-sm font-medium">{cluster.title || "(无标题)"}</span>
                          <span className="mt-0.5 block truncate text-[11px] text-muted-foreground">
                            {cluster.accountCount} 个账号 · {cluster.copyCount} 份副本 · {formatBytes(cluster.bytes)}
                          </span>
                        </span>
                        <Badge variant={cluster.covered ? "success" : "warning"} className="text-[10px]">
                          {cluster.covered ? "已建立关联" : "未建立关联"}
                        </Badge>
                      </button>
                      {isOpen && (
                        <ul className="space-y-0.5 border-t bg-muted/30 px-2 py-1.5 text-[11px] text-muted-foreground">
                          {cluster.members.map((member) => (
                            <li key={member.sessionId} className="flex flex-wrap items-center justify-between gap-2">
                              <span className="truncate">
                                {member.uid.slice(0, 8)}… / {member.sessionId.slice(0, 8)}…
                              </span>
                              <span className="tabular-nums">
                                {formatBytes(member.bytes)} {member.covered ? "· 已关联" : "· 独立"}
                              </span>
                            </li>
                          ))}
                          {cluster.copyCount > cluster.members.length && (
                            <li>…另有 {cluster.copyCount - cluster.members.length} 份未列出</li>
                          )}
                        </ul>
                      )}
                    </div>
                  );
                })}
                {(result.clusters ?? []).length === 0 && (
                  <p className="rounded-md border border-dashed px-3 py-6 text-center text-sm text-muted-foreground">
                    没有发现跨账号的同一段对话
                  </p>
                )}
              </div>

              <div className="flex justify-end gap-2">
                <Button variant="outline" size="sm" onClick={() => void run(true)} disabled={busy}>
                  重新普查
                </Button>
              </div>
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
