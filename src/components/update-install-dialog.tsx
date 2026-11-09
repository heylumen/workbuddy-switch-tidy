import { useEffect, useState } from "react";
import { ExternalLink, Loader2 } from "lucide-react";

import { DemoAction } from "@/components/demo-action";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { GITHUB_RELEASE_URL, openReleaseUrl } from "@/lib/update";
import { useUpdateState } from "@/lib/use-update-state";

interface UpdateInstallDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * 统一更新弹窗：只展示 Rust 更新状态机（与托盘菜单同源）的阶段。
 *
 * 单文件版不提供应用内下载/安装；发现新版本时只打开发布页手动下载。
 */
export function UpdateInstallDialog({ open, onOpenChange }: UpdateInstallDialogProps) {
  const snapshot = useUpdateState();
  const [actionError, setActionError] = useState<string | null>(null);

  const phase = snapshot.phase;
  const targetVersion = snapshot.latest ?? "新版本";
  const error = actionError ?? snapshot.message ?? "未知更新错误";
  const showRelease =
    phase === "available" ||
    phase === "downloading" ||
    phase === "readyToRestart" ||
    phase === "error";

  useEffect(() => {
    if (!open) return;
    setActionError(null);
  }, [open]);

  async function openRelease() {
    try {
      await openReleaseUrl(GITHUB_RELEASE_URL);
    } catch (e) {
      setActionError(api.asError(e));
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {phase === "checking" && "正在检查更新"}
            {(phase === "available" ||
              phase === "downloading" ||
              phase === "readyToRestart") &&
              "发现新版本"}
            {(phase === "idle" || phase === "upToDate") && "当前已是最新版本"}
            {phase === "error" && "更新检查失败"}
          </DialogTitle>
          <DialogDescription>
            {phase === "checking" && "正在检查 GitHub Release 中的版本信息，请稍候。"}
            {phase === "available" &&
              `发现新版本 v${targetVersion}，请在 GitHub Release 页面手动下载。`}
            {(phase === "downloading" || phase === "readyToRestart") &&
              `新版本 v${targetVersion} 请在 GitHub Release 页面手动下载。`}
            {(phase === "idle" || phase === "upToDate") && "没有发现高于当前版本的版本。"}
            {phase === "error" && "更新检查未完成，你仍然可以从 GitHub Release 页面手动下载。"}
          </DialogDescription>
        </DialogHeader>

        {phase === "checking" && (
          <div className="flex items-center gap-2 rounded-md bg-muted/50 p-3 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" />
            正在连接公开 Release 更新源…
          </div>
        )}

        {(phase === "available" ||
          phase === "downloading" ||
          phase === "readyToRestart") && (
          <div className="rounded-md border p-3 text-sm">
            v{targetVersion} 已确认可用。本版为单文件便携版，请从发布页下载后替换当前文件。
          </div>
        )}

        {phase === "error" && (
          <div className="space-y-2 rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm">
            <p className="text-destructive">{error}</p>
            <p className="break-all text-xs text-muted-foreground">{GITHUB_RELEASE_URL}</p>
          </div>
        )}

        <DialogFooter>
          {showRelease ? (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                关闭
              </Button>
              <DemoAction>
                <Button onClick={() => void openRelease()}>
                  <ExternalLink />
                  打开发布页
                </Button>
              </DemoAction>
            </>
          ) : (
            <Button variant="default" onClick={() => onOpenChange(false)}>
              关闭
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
