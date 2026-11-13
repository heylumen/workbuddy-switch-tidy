import { useEffect, useMemo, useState } from "react";
import { Copy, Loader2 } from "lucide-react";
import { toast } from "sonner";

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

/** 本地工作记忆复制（#209）：只复制 MEMORY.md，不做云端同步。 */
export function CopyMemoryDialog({ open, onOpenChange, accounts, defaultSourceId }: Props) {
  const [sourceId, setSourceId] = useState("");
  const [targetId, setTargetId] = useState("");
  const [busy, setBusy] = useState(false);

  const candidates = useMemo(
    () => accounts.filter((account) => Boolean(account.uid)),
    [accounts],
  );

  useEffect(() => {
    if (!open) return;
    const source =
      candidates.find((account) => account.id === defaultSourceId) ?? candidates[0];
    const target = candidates.find((account) => account.id !== source?.id);
    setSourceId(source?.id ?? "");
    setTargetId(target?.id ?? "");
    setBusy(false);
  }, [open, defaultSourceId, candidates]);

  const source = candidates.find((account) => account.id === sourceId);
  const target = candidates.find((account) => account.id === targetId);
  const canCopy = Boolean(source && target && source.id !== target.id && !busy);

  async function handleCopy() {
    if (!source || !target) return;
    setBusy(true);
    try {
      const result = await api.copyAccountMemory(source.id, target.id);
      toast.success(
        result.changed ? "工作记忆已复制" : "目标账号已是相同内容",
        {
          description: result.changed
            ? result.backup
              ? `已写入 ${result.bytes} 字节；原文件备份：${result.backup}`
              : `已写入 ${result.bytes} 字节`
            : "内容一致，未写入文件",
        },
      );
      onOpenChange(false);
    } catch (error) {
      toast.error("复制工作记忆失败", { description: api.asError(error) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!busy) onOpenChange(next); }}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>复制工作记忆</DialogTitle>
          <DialogDescription>
            把来源账号的本地 <code>MEMORY.md</code> 复制到目标账号；不涉及云端记忆，目标已有内容会先备份。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-1">
          {candidates.length < 2 ? (
            <p className="rounded-lg border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-sm text-muted-foreground">
              至少需要两个具有 UID 的账号才能复制工作记忆。
            </p>
          ) : null}
          <div className="space-y-1.5">
            <Label htmlFor="memory-source">来源账号</Label>
            <Select value={sourceId} onValueChange={setSourceId}>
              <SelectTrigger id="memory-source">
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
            <Label htmlFor="memory-target">目标账号</Label>
            <Select value={targetId} onValueChange={setTargetId}>
              <SelectTrigger id="memory-target">
                <SelectValue placeholder="选择目标账号" />
              </SelectTrigger>
              <SelectContent>
                {candidates
                  .filter((account) => account.id !== sourceId)
                  .map((account) => (
                    <SelectItem key={account.id} value={account.id}>
                      {displayName(account)} · {variantLabel(account.variant ?? "cn")}
                    </SelectItem>
                  ))}
              </SelectContent>
            </Select>
          </div>

          <p className="rounded-lg border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-xs text-muted-foreground">
            只复制 <code>MEMORY.md</code>，不会复制 IDENTITY / SOUL / USER，也不会同步到云端。
            <br />
            执行前请先完全退出 WorkBuddy（国内版与国际版）；后端会拒绝运行中的写入。
          </p>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={() => void handleCopy()} disabled={!canCopy}>
            {busy ? <Loader2 className="animate-spin" /> : <Copy />}
            复制
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
