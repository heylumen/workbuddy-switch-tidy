import { useCallback, useEffect, useState } from "react";
import { FileDown, Loader2, Search } from "lucide-react";
import { toast } from "sonner";

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

function formatTime(at: number): string {
  if (!at) return "—";
  const date = new Date(at);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 全局会话搜索：标题优先，可选正文；结果可直接导出 Markdown。 */
export function SessionSearchDialog({ variant }: { variant: WbVariant }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [includeBody, setIncludeBody] = useState(false);
  // 默认只搜当前档位；勾选后同时搜另一档位（会话库按档位分开）
  const [bothVariants, setBothVariants] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<api.SessionSearchResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [exporting, setExporting] = useState<string | null>(null);

  const run = useCallback(async () => {
    const trimmed = query.trim();
    if (!trimmed) {
      setResult(null);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const other: "cn" | "ai" = variant === "ai" ? "cn" : "ai";
      const [primary, secondary] = bothVariants
        ? await Promise.all([
            api.searchSessions(variant, trimmed, 30, includeBody),
            api.searchSessions(other, trimmed, 30, includeBody).catch(() => null),
          ])
        : [await api.searchSessions(variant, trimmed, 30, includeBody), null];
      if (!primary.ok) {
        setError(primary.reason || "搜索失败");
        setResult(null);
        return;
      }
      if (secondary?.ok) {
        const merged = [...primary.items, ...secondary.items].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, 60);
        setResult({ ...primary, items: merged, total: merged.length });
      } else {
        setResult(primary);
      }
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }, [query, variant, includeBody, bothVariants]);

  useEffect(() => {
    if (!open) return;
    const timer = window.setTimeout(() => void run(), 320);
    return () => window.clearTimeout(timer);
  }, [open, run]);

  async function exportHit(hit: api.SessionSearchHit) {
    if (!hit.accountId) {
      toast.error("无法导出", { description: "该会话未关联到本工具已知账号" });
      return;
    }
    setExporting(hit.sessionId);
    try {
      const data = await api.exportSessionMarkdown(hit.accountId, hit.sessionId, hit.title);
      if (!data.ok || !data.markdown) {
        toast.error("导出失败", { description: data.reason || "未知错误" });
        return;
      }
      const blob = new Blob([data.markdown], { type: "text/markdown;charset=utf-8" });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = data.fileName || `${hit.sessionId}.md`;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
      if (data.readError) {
        toast.warning(`已导出 ${data.messages ?? 0} 条消息，但内容不完整`, { description: data.readError });
      } else {
        toast.success(`已导出 ${data.messages ?? 0} 条消息`);
      }
    } catch (e) {
      toast.error("导出失败", { description: api.asError(e) });
    } finally {
      setExporting(null);
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="shrink-0">
          <Search />搜索会话
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>搜索会话</DialogTitle>
          <DialogDescription>
            默认只搜当前档位（国内版/国际版会话库分开）。可勾选"含另一档位"合并搜索；
            勾选"搜索正文"后额外扫描正文（仅最近 200 个、单个 8 MB 以内，最多返回 30 条/档位）。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <div className="relative flex-1">
              <Search className="pointer-events-none absolute left-2.5 top-2.5 size-4 text-muted-foreground" />
              <input
                className="h-9 w-full rounded-md border bg-background pl-8 pr-3 text-sm outline-none focus:ring-2 focus:ring-primary/30"
                placeholder="输入关键词…"
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                autoFocus
              />
            </div>
            <div className="flex shrink-0 flex-col gap-1 text-xs text-muted-foreground">
              <label className="flex items-center gap-1.5">
                <input
                  type="checkbox"
                  className="size-4 accent-primary"
                  checked={includeBody}
                  onChange={(event) => setIncludeBody(event.target.checked)}
                />
                搜索正文
              </label>
              <label className="flex items-center gap-1.5">
                <input
                  type="checkbox"
                  className="size-4 accent-primary"
                  checked={bothVariants}
                  onChange={(event) => setBothVariants(event.target.checked)}
                />
                含另一档位
              </label>
            </div>
          </div>

          {error && (
            <p className="rounded-md border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-xs text-destructive">
              {error}
            </p>
          )}

          {busy && (
            <p className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" /> 搜索中{includeBody ? "（正在扫描正文，可能需要几秒）" : "…"}
            </p>
          )}

          {result && !busy && (
            <div className="space-y-1.5">
              <p className="text-xs text-muted-foreground">
                命中 {result.total} 条
                {result.bodyScanned ? ` · 扫描正文 ${result.bodyScanned} 个文件` : ""}
                {result.truncated ? " · 已达上限，结果可能不完整" : ""}
              </p>
              <div className="max-h-[46vh] space-y-1 overflow-y-auto">
                {result.items.length === 0 && (
                  <p className="rounded-md border border-dashed px-3 py-6 text-center text-sm text-muted-foreground">
                    没有匹配的会话
                  </p>
                )}
                {result.items.map((hit) => (
                  <div key={hit.sessionId} className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2">
                    <div className="min-w-0">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <span className="truncate text-sm font-medium">{hit.title || "(无标题)"}</span>
                        <Badge variant={hit.match === "body" ? "warning" : "outline"} className="text-[10px]">
                          {hit.match === "body" ? "正文命中" : "标题命中"}
                        </Badge>
                      </div>
                      <p className="mt-0.5 text-[11px] text-muted-foreground">
                        {hit.accountName || hit.uid.slice(0, 8)} · {formatTime(hit.updatedAt)}
                        {hit.cwd ? ` · ${hit.cwd}` : ""}
                      </p>
                    </div>
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 px-2 text-xs"
                      disabled={exporting === hit.sessionId || !hit.accountId}
                      onClick={() => void exportHit(hit)}
                    >
                      {exporting === hit.sessionId ? (
                        <Loader2 className="size-3.5 animate-spin" />
                      ) : (
                        <FileDown className="size-3.5" />
                      )}
                      导出
                    </Button>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
