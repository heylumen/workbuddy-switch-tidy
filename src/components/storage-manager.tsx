import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { HardDrive, Loader2, RefreshCw, Trash2, Undo2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value >= 100 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}

function formatTime(at: number): string {
  if (!at) return "—";
  const date = new Date(at);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/**
 * 存储管理：按类别查看体积，可回收项「移入回收站」（不物理删除业务数据）。
 *
 * 安全边界由后端保证：前端只传类别 id；会话正文 / 程序本体不在可回收清单内；
 * 客户端侧目录需要先退出客户端。
 */
export function StorageManager() {
  const [variant, setVariant] = useState<"cn" | "ai">("cn");
  const [scan, setScan] = useState<api.StorageScan | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [purgeConfirm, setPurgeConfirm] = useState(false);
  const [orphans, setOrphans] = useState<api.OrphanBodyScan | null>(null);

  const load = useCallback(
    async (target: "cn" | "ai" = variant) => {
      setBusy("scan");
      setError(null);
      try {
        const [result, orphan] = await Promise.all([
          api.storageScan(target),
          api.storageOrphanScan(target).catch(() => null),
        ]);
        if (!result.ok) {
          setError(result.reason || "扫描失败");
          return;
        }
        setScan(result);
        setOrphans(orphan);
      } catch (e) {
        setError(api.asError(e));
      } finally {
        setBusy(null);
      }
    },
    [variant],
  );

  useEffect(() => {
    void load(variant);
  }, [variant, load]);

  async function moveCategory(category: api.StorageCategory) {
    if (!scan) return;
    const confirmed = window.confirm(
      `将「${category.label}」（${formatBytes(category.bytes)}）移入回收站？\n\n` +
        `文件会被移动到 ~/.wb-switch/trash，可在本页恢复；不会被直接删除。` +
        (category.needsClientClosed ? "\n\n客户端需已退出。" : ""),
    );
    if (!confirmed) return;
    setBusy(category.id);
    setError(null);
    try {
      const result = await api.storageMoveToTrash(variant, category.id);
      if (!result.ok) {
        setError(result.reason || "移入回收站失败");
        return;
      }
      toast.success(`已移入回收站：${category.label}`, {
        description: `${formatBytes(category.bytes)} · 可随时恢复`,
      });
      await load();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  async function restore(entry: api.TrashEntry) {
    setBusy(entry.id);
    setError(null);
    try {
      const result = await api.storageRestoreTrash(entry.id);
      if (!result.ok) {
        setError(result.reason || "恢复失败");
        return;
      }
      toast.success("已恢复到原位", { description: result.restoredTo });
      await load();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  async function trashOrphans() {
    if (!orphans?.count) return;
    const confirmed = window.confirm(
      `将 ${orphans.count} 个「已删除会话」的正文（${formatBytes(orphans.bytes)}）移入回收站？\n\n` +
        "这些会话在客户端已删除，但正文文件仍占磁盘。移入回收站后可整条恢复；请先退出客户端。",
    );
    if (!confirmed) return;
    setBusy("orphan");
    setError(null);
    try {
      const result = await api.storageOrphanTrash(variant);
      if (!result.ok) {
        setError(result.reason || "移入回收站失败");
        return;
      }
      toast.success(`已移入回收站 ${result.moved ?? 0} 个正文文件`, {
        description: `释放 ${formatBytes(result.bytes ?? 0)}；可在下方回收站恢复`,
      });
      await load();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  async function purge(days: number) {
    if (days === 0 && !purgeConfirm) {
      setPurgeConfirm(true);
      return;
    }
    setBusy(`purge-${days}`);
    setError(null);
    try {
      const result = await api.storagePurgeTrash(days);
      if (!result.ok) {
        setError("清空回收站失败");
        return;
      }
      toast.success(`已清空 ${result.purged ?? 0} 项`, {
        description: `释放 ${formatBytes(result.bytes ?? 0)}（此操作不可恢复）`,
      });
      setPurgeConfirm(false);
      await load();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <HardDrive className="size-4 text-muted-foreground" />
          <span className="text-sm font-medium">存储占用</span>
          {scan && (
            <Badge variant="outline" className="tabular-nums">
              可回收 {formatBytes(scan.trashableBytes)}
            </Badge>
          )}
        </div>
        <div className="flex items-center gap-2">
          <div className="flex overflow-hidden rounded-md border">
            {(["cn", "ai"] as const).map((value) => (
              <button
                key={value}
                type="button"
                onClick={() => setVariant(value)}
                className={cn(
                  "px-2.5 py-1 text-xs transition-colors",
                  variant === value ? "bg-primary text-primary-foreground" : "bg-transparent text-muted-foreground",
                )}
              >
                {value === "cn" ? "国内版" : "国际版"}
              </button>
            ))}
          </div>
          <Button
            size="sm"
            variant="outline"
            disabled={busy === "scan"}
            onClick={() => void load(variant)}
          >
            {busy === "scan" ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
            重新扫描
          </Button>
        </div>
      </div>

      {error && (
        <p className="rounded-md border border-destructive/30 bg-destructive/5 p-2 text-xs text-destructive">{error}</p>
      )}

      {!scan && busy === "scan" && (
        <p className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" /> 正在统计体积…
        </p>
      )}

      {scan && (
        <div className="space-y-1.5">
          {scan.categories
            .filter((category) => category.exists || category.bytes > 0)
            .map((category) => (
              <div
                key={category.id}
                className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2"
              >
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="text-sm">{category.label}</span>
                    <span className="tabular-nums text-xs text-muted-foreground">
                      {formatBytes(category.bytes)} · {category.files} 个文件
                    </span>
                    {!category.trashable && <Badge variant="secondary">不可回收</Badge>}
                    {category.trashable && category.needsClientClosed && (
                      <Badge variant="outline">需退出客户端</Badge>
                    )}
                  </div>
                  <p className="mt-0.5 text-[11px] text-muted-foreground">{category.hint}</p>
                </div>
                {category.trashable && (
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={busy === category.id || category.bytes === 0}
                    onClick={() => void moveCategory(category)}
                  >
                    {busy === category.id ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : (
                      <Trash2 className="size-3.5" />
                    )}
                    移入回收站
                  </Button>
                )}
              </div>
            ))}
        </div>
      )}

      {orphans?.ok && (orphans.count ?? 0) > 0 && (
        <div className="rounded-md border border-amber-300/60 bg-amber-50/40 p-2 dark:bg-amber-950/10">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="text-sm">
              已删除会话的正文残留
              <span className="ml-2 tabular-nums text-xs text-muted-foreground">
                {orphans.count} 个 · {formatBytes(orphans.bytes)}
              </span>
            </span>
            <Button size="sm" variant="outline" disabled={busy === "orphan"} onClick={() => void trashOrphans()}>
              {busy === "orphan" ? <Loader2 className="size-3.5 animate-spin" /> : <Trash2 className="size-3.5" />}
              移入回收站
            </Button>
          </div>
          <p className="mt-1 text-[11px] text-muted-foreground">
            这些会话在客户端库里已标记删除，但正文 jsonl 仍占磁盘；移入回收站后可整条恢复（需先退出客户端）。
          </p>
        </div>
      )}

      {scan && (
        <div className="rounded-md border p-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="text-sm">
              回收站
              <span className="ml-2 tabular-nums text-xs text-muted-foreground">
                {scan.trash.entries.length} 项 · {formatBytes(scan.trash.bytes)}
              </span>
            </span>
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                disabled={!scan.trash.entries.length || busy === "purge-30"}
                onClick={() => void purge(30)}
              >
                {busy === "purge-30" ? <Loader2 className="size-3.5 animate-spin" /> : <Trash2 className="size-3.5" />}
                清空 30 天前
              </Button>
              <Button
                size="sm"
                variant={purgeConfirm ? "destructive" : "outline"}
                disabled={!scan.trash.entries.length || busy === "purge-0"}
                onClick={() => void purge(0)}
              >
                {purgeConfirm ? "确认全部清空" : "全部清空"}
              </Button>
            </div>
          </div>
          {scan.trash.entries.length > 0 && (
            <div className="mt-2 space-y-1">
              {scan.trash.entries.map((entry) => (
                <div key={entry.id} className="flex flex-wrap items-center justify-between gap-2 text-xs">
                  <span className="min-w-0 truncate">
                    {entry.label}
                    <span className="ml-2 text-muted-foreground">
                      {formatBytes(entry.bytes)} · {formatTime(entry.createdAt)}
                    </span>
                  </span>
                  <Button size="sm" variant="ghost" disabled={busy === entry.id} onClick={() => void restore(entry)}>
                    {busy === entry.id ? <Loader2 className="size-3.5 animate-spin" /> : <Undo2 className="size-3.5" />}
                    恢复
                  </Button>
                </div>
              ))}
            </div>
          )}
          {!scan.trash.entries.length && (
            <p className="mt-1 text-[11px] text-muted-foreground">回收站为空。移入的项目会保留在这里，可随时恢复。</p>
          )}
        </div>
      )}

      <p className="text-[11px] text-muted-foreground">
        说明：会话正文（projects）、工作区文件、文件历史与程序本体不在可回收范围；会话整理请用「重复审计 / 折叠同名」。
        回收站保留 30 天（可用环境变量 WB_SWITCH_TRASH_RETENTION_DAYS 调整，0 为关闭），过期会自动清理；「清空」会立即删除。
      </p>
    </div>
  );
}
