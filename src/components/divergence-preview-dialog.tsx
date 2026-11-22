import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";

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
import type { SessionGroupMemberDetail, WbVariant } from "@/lib/types";

/**
 * 消息级分叉预览：把「从哪一条消息开始不一样」摊开。
 *
 * 组详情里的分支图只表达「谁和谁不同」；这里给出每个副本共同前缀之后的具体消息。
 */
export function DivergencePreviewDialog({
  groupId,
  variant,
  members,
}: {
  groupId: string;
  variant: WbVariant;
  members: SessionGroupMemberDetail[];
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [data, setData] = useState<api.DivergencePreview | null>(null);

  useEffect(() => {
    if (!open || data || busy) return;
    let cancelled = false;
    setBusy(true);
    setError(null);
    api
      .divergencePreview(groupId, variant, 10)
      .then((result) => {
        if (cancelled) return;
        if (!result.ok) setError(result.reason || "对比失败");
        else setData(result);
      })
      .catch((e) => {
        if (!cancelled) setError(api.asError(e));
      })
      .finally(() => {
        if (!cancelled) setBusy(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, data, busy, groupId, variant]);

  const nameOf = (uid: string) =>
    members.find((member) => member.uid === uid)?.accountName ?? `${uid.slice(0, 8)}…`;

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="h-7 gap-1 px-2 text-xs">
          消息级差异
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>消息级差异</DialogTitle>
          <DialogDescription>
            各副本共同前缀之后的具体消息（只读）。共同前缀指所有副本逐条一致的部分。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          {busy && (
            <p className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" /> 正在逐条比对…
            </p>
          )}
          {error && (
            <p className="rounded-md border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-xs text-destructive">
              {error}
            </p>
          )}

          {data && !busy && (
            <>
              <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                <span>可对比副本 {data.comparable ?? 0} 份</span>
                <span>·</span>
                <span>共同前缀 {data.commonPrefix ?? 0} 条消息</span>
                {data.sampled ? (
                  <Badge variant="warning" className="text-[10px]">
                    超大副本：仅比对开头若干条
                  </Badge>
                ) : null}
                {data.reason ? <Badge variant="warning" className="text-[10px]">{data.reason}</Badge> : null}
              </div>

              <div className="max-h-[52vh] space-y-2 overflow-y-auto">
                {(data.members ?? []).map((member) => (
                  <div key={member.sessionId} className="rounded-md border">
                    <div className="flex flex-wrap items-center justify-between gap-2 border-b bg-muted/40 px-2 py-1.5">
                      <span className="text-sm font-medium">{nameOf(member.uid)}</span>
                      <span className="text-[11px] text-muted-foreground">
                        共 {member.messages} 条 · 超出共同前缀{" "}
                        <span className="font-medium text-foreground">{member.beyondCommon}</span> 条
                      </span>
                    </div>
                    <ul className="space-y-1 p-2 text-xs">
                      {member.items.length === 0 && (
                        <li className="text-muted-foreground">与其它副本完全一致，没有独有消息。</li>
                      )}
                      {member.items.map((item, index) => (
                        <li key={index} className="rounded bg-muted/40 px-2 py-1">
                          <span className="mr-1.5 font-medium text-foreground">{item.role}</span>
                          <span className="whitespace-pre-wrap break-words text-muted-foreground">{item.text}</span>
                        </li>
                      ))}
                      {member.beyondCommon > member.items.length && (
                        <li className="text-muted-foreground">
                          …另有 {member.beyondCommon - member.items.length} 条未列出
                        </li>
                      )}
                    </ul>
                  </div>
                ))}
              </div>

              {(data.unavailable ?? []).length > 0 && (
                <div className="rounded-md border border-amber-500/30 bg-amber-500/5 p-2 text-[11px] text-muted-foreground">
                  <p className="mb-1 font-medium text-foreground">以下副本未能参与对比：</p>
                  <ul className="space-y-0.5">
                    {(data.unavailable ?? []).map((item) => (
                      <li key={item.sessionId}>
                        {nameOf(item.uid)} · {item.reason}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
