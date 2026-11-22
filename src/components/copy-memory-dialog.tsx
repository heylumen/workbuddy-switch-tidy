import { useCallback, useEffect, useMemo, useState } from "react";
import { Copy, Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";
import { displayName } from "@/lib/account-display";
import type { AccountMeta } from "@/lib/types";
import { variantLabel } from "@/lib/variant";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  accounts: AccountMeta[];
  defaultSourceId?: string;
}

function formatBytes(bytes: number): string {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB"];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value >= 100 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}

/**
 * 账号资料迁移：人格（IDENTITY/SOUL/USER/BOOTSTRAP）、本地工作记忆（MEMORY.md）、
 * 偏好设置与最近专家记录。
 *
 * 连接器状态**不在可迁移范围**：它按账号加密并绑定身份（`.master.key` + `identityKey`），
 * 跨账号复制既解不开也有泄露风险；云端记忆缓存也不迁移（搬过去会被误认为本账号记忆）。
 */
export function CopyMemoryDialog({ open, onOpenChange, accounts, defaultSourceId }: Props) {
  const [sourceId, setSourceId] = useState("");
  const [targetId, setTargetId] = useState("");
  const [scan, setScan] = useState<api.AccountMergeScan | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [overwrite, setOverwrite] = useState(false);
  const [busy, setBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const candidates = useMemo(
    () => accounts.filter((account) => Boolean(account.uid)),
    [accounts],
  );

  useEffect(() => {
    if (!open) return;
    const source = candidates.find((account) => account.id === defaultSourceId) ?? candidates[0];
    const target = candidates.find((account) => account.id !== source?.id);
    setSourceId(source?.id ?? "");
    setTargetId(target?.id ?? "");
    setScan(null);
    setSelected([]);
    setOverwrite(false);
    setBusy(false);
    setError(null);
  }, [open, defaultSourceId, candidates]);

  const runScan = useCallback(async () => {
    if (!sourceId || !targetId || sourceId === targetId) return;
    setScanning(true);
    setError(null);
    try {
      const result = await api.accountMergeScan(sourceId, targetId);
      if (!result.ok) {
        setError(result.reason || "扫描失败");
        setScan(null);
        return;
      }
      setScan(result);
      setSelected(result.items.filter((item) => item.sourceExists).map((item) => item.id));
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setScanning(false);
    }
  }, [sourceId, targetId]);

  useEffect(() => {
    if (!open) return;
    void runScan();
  }, [open, runScan]);

  const source = candidates.find((account) => account.id === sourceId);
  const target = candidates.find((account) => account.id === targetId);
  const canApply = Boolean(source && target && source.id !== target.id && selected.length && !busy);

  function toggle(id: string) {
    setSelected((current) =>
      current.includes(id) ? current.filter((item) => item !== id) : [...current, id],
    );
  }

  async function handleApply() {
    if (!source || !target) return;
    setBusy(true);
    setError(null);
    try {
      const result = await api.accountMergeApply(source.id, target.id, selected, overwrite);
      if (!result.ok) {
        setError(result.reason || "迁移失败");
        return;
      }
      const copied = result.copied?.length ?? 0;
      const skipped = result.skipped?.length ?? 0;
      toast.success(`已迁移 ${copied} 项到「${displayName(target)}」`, {
        description: [skipped ? `${skipped} 项跳过` : "", result.backup ? "目标原文件已备份" : ""]
          .filter(Boolean)
          .join(" · ") || undefined,
      });
      onOpenChange(false);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!busy) onOpenChange(next); }}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>迁移账号资料</DialogTitle>
          <DialogDescription>
            把来源账号的<span className="font-medium">人格 / 本地工作记忆 / 偏好 / 专家记录</span>
            复制到目标账号；目标已有内容会先备份。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-1">
          {candidates.length < 2 && (
            <p className="rounded-lg border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-sm text-muted-foreground">
              至少需要两个具有 UID 的账号才能迁移资料。
            </p>
          )}

          <div className="grid gap-3 sm:grid-cols-2">
            <div className="space-y-1.5">
              <Label htmlFor="merge-source">来源账号</Label>
              <Select value={sourceId} onValueChange={setSourceId}>
                <SelectTrigger id="merge-source">
                  <SelectValue placeholder="选择来源账号" />
                </SelectTrigger>
                <SelectContent>
                  {candidates.map((account) => (
                    <SelectItem key={account.id} value={account.id}>
                      {displayName(account)} · {variantLabel(account.variant ?? "cn")}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="merge-target">目标账号</Label>
              <Select value={targetId} onValueChange={setTargetId}>
                <SelectTrigger id="merge-target">
                  <SelectValue placeholder="选择目标账号" />
                </SelectTrigger>
                <SelectContent>
                  {candidates.map((account) => (
                    <SelectItem key={account.id} value={account.id}>
                      {displayName(account)} · {variantLabel(account.variant ?? "cn")}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          {error && (
            <p className="rounded-md border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-xs text-destructive">
              {error}
            </p>
          )}

          {scanning && (
            <p className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" /> 正在扫描可迁移条目…
            </p>
          )}

          {scan && !scanning && (
            <div className="space-y-1.5">
              {scan.items.map((item) => {
                const disabled = !item.sourceExists;
                const checked = selected.includes(item.id);
                return (
                  <label
                    key={item.id}
                    className={
                      "flex cursor-pointer items-start gap-2 rounded-md border p-2 text-sm " +
                      (disabled ? "opacity-50" : "")
                    }
                  >
                    <input
                      type="checkbox"
                      className="mt-0.5 size-4 accent-primary"
                      checked={checked}
                      disabled={disabled || busy}
                      onChange={() => toggle(item.id)}
                    />
                    <span className="min-w-0 flex-1">
                      <span className="flex flex-wrap items-center gap-2">
                        <span>{item.label}</span>
                        {item.sourceExists ? (
                          <span className="text-xs tabular-nums text-muted-foreground">
                            {formatBytes(item.sourceBytes)}
                          </span>
                        ) : (
                          <Badge variant="secondary">来源无此项</Badge>
                        )}
                        {item.targetExists && <Badge variant="outline">目标已有</Badge>}
                      </span>
                      <span className="mt-0.5 block text-[11px] text-muted-foreground">{item.hint}</span>
                    </span>
                  </label>
                );
              })}

              <label className="flex items-center gap-2 pt-1 text-xs text-muted-foreground">
                <input
                  type="checkbox"
                  className="size-4 accent-primary"
                  checked={overwrite}
                  disabled={busy}
                  onChange={(event) => setOverwrite(event.target.checked)}
                />
                覆盖目标已有的同名文件（不勾选时自动跳过；覆盖前会备份）
              </label>

              {scan.notMigratable.length > 0 && (
                <details className="pt-1 text-[11px] text-muted-foreground">
                  <summary className="cursor-pointer">哪些内容不迁移？</summary>
                  <ul className="mt-1 list-disc space-y-1 pl-4">
                    {scan.notMigratable.map((line) => (
                      <li key={line}>{line}</li>
                    ))}
                  </ul>
                </details>
              )}
            </div>
          )}

          <p className="rounded-lg border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-xs text-muted-foreground">
            执行前请先完全退出 WorkBuddy（国内版与国际版）；后端会拒绝运行中的写入。
          </p>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={() => void handleApply()} disabled={!canApply}>
            {busy ? <Loader2 className="size-4 animate-spin" /> : <Copy className="size-4" />}
            迁移所选 {selected.length} 项
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
