import { useEffect, useRef, useState } from "react";
import { FileUp, Loader2 } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { DEFAULT_VARIANT, variantLabel } from "@/lib/variant";
import type { ImportPreviewAccount, WbVariant } from "@/lib/types";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 导入完成后回调（参数为导入结果计数，附所选账号中的加密凭据数）。 */
  onImported?: (result: {
    imported: number;
    skipped: number;
    overwritten: number;
    encrypted: number;
  }) => void;
  /** 当前档位；仅用于文案，导入结果按文件内各账号自身的档位归类。 */
  variant?: WbVariant;
}

/**
 * 导入预览账号展示名（脱敏展示：昵称/邮箱/uid）。
 * 只接受字符串：旧后端可能把信封对象透传进来，渲染对象会触发 React error #31（白屏）。
 */
function previewLabel(a: ImportPreviewAccount): string {
  const asLabel = (v: unknown) => (typeof v === "string" ? v : "");
  return asLabel(a.nickname) || asLabel(a.email) || asLabel(a.uid) || `第 ${a.index + 1} 项`;
}

/** 导入账号弹框：选 JSON 文件 → 后端解析预览 → 勾选账号 → 导入合并。 */
export function ImportAccountsDialog({
  open,
  onOpenChange,
  onImported,
  variant = DEFAULT_VARIANT,
}: Props) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [fileName, setFileName] = useState("");
  const [fileText, setFileText] = useState("");
  // 桌面端按路径交给 Rust 解析，避免大 JSON 在 WebView 主线程解析导致卡死。
  const [filePath, setFilePath] = useState("");
  const [preview, setPreview] = useState<ImportPreviewAccount[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [parsing, setParsing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");/** 加密备份口令：解密在 Rust 侧完成，口令不落盘。 */
  const [password, setPassword] = useState("");

  useEffect(() => {
    if (open) {
      setFileName("");
      setFileText("");
      setFilePath("");
      setPreview([]);
      setSelected(new Set());
      setParsing(false);
      setBusy(false);
      setError("");
    }
  }, [open]);

  async function chooseFile() {
    if (!api.isDesktop()) {
      fileInputRef.current?.click();
      return;
    }
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({ multiple: false, filters: [{ name: "JSON", extensions: ["json"] }] });
      if (typeof picked !== "string" || !picked) return;
      setFilePath(picked);
      setFileText("");
      setFileName(picked.split(/[\\/]/).pop() || picked);
      setParsing(true);
      setError("");
      try {
        const previewResult = await api.previewImportFile(picked, password.trim() || undefined);
        setPreview(previewResult.accounts);
        setSelected(new Set(previewResult.accounts.map((a) => a.index)));
      } catch (err) {
        setPreview([]);
        setSelected(new Set());
        setFilePath("");
        setError(api.asError(err));
      } finally {
        setParsing(false);
      }
    } catch (e) {
      setError(api.asError(e));
    }
  }

  function onFileChange(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    // 清空 value，允许再次选择同一文件
    e.target.value = "";
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      const text = String(reader.result ?? "");
      if (text.length > 8 * 1024 * 1024) {
        setError("文件较大（超过 8 MB），浏览器内解析可能短暂卡顿；桌面版会交由后端解析。");
      }
      setFileName(file.name);
      setFileText(text);
      setParsing(true);
      setError("");
      api
        .previewImportAccounts(text)
        .then((res) => {
          setPreview(res.accounts);
          setSelected(new Set(res.accounts.map((a) => a.index)));
        })
        .catch((err) => {
          setPreview([]);
          setSelected(new Set());
          setError(api.asError(err));
        })
        .finally(() => setParsing(false));
    };
    reader.readAsText(file);
  }

  const allSelected = preview.length > 0 && selected.size === preview.length;

  function toggleAll() {
    setSelected(allSelected ? new Set() : new Set(preview.map((a) => a.index)));
  }

  function toggle(index: number) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  }

  async function doImport() {
    if (busy || parsing || (!fileText && !filePath) || selected.size === 0) return;
    setBusy(true);
    setError("");
    try {
      // 所选账号中的加密凭据数：随导入结果上报，供宿主附加能力限制提醒。
      const encrypted = preview.filter((a) => selected.has(a.index) && a.encrypted).length;
      const res = filePath
        ? await api.importAccountsFile(filePath, [...selected], password.trim() || undefined)
        : await api.importAccounts(fileText, [...selected]);
      onImported?.({ ...res, encrypted });
      onOpenChange(false);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="min-w-0 overflow-x-hidden">
        <DialogHeader>
          <DialogTitle>导入账号</DialogTitle>
          <DialogDescription>
            选择 JSON 文件，勾选要导入的账号。
            {variant === "ai" &&
              `备份中的国内版与国际版账号按各自档位归类，不随当前${variantLabel(variant)}改变。`}
          </DialogDescription>
        </DialogHeader>

        <input
          ref={fileInputRef}
          type="file"
          accept=".json,application/json"
          className="hidden"
          onChange={onFileChange}
        />

        <div className="flex items-center gap-2">
          <Button variant="outline" onClick={chooseFile} disabled={busy}>
            <FileUp />
            选择文件
          </Button>
          {fileName && <span className="truncate text-xs text-muted-foreground">{fileName}</span>}
        </div>

        <div className="space-y-1.5">
          <label htmlFor="import-password" className="text-sm font-medium">
            口令（仅加密备份需要）
          </label>
          <Input
            id="import-password"
            type="password"
            autoComplete="off"
            placeholder="明文备份留空即可"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            disabled={busy || parsing}
          />
        </div>

        {parsing && (
          <div className="flex items-center gap-2 py-4 text-sm text-muted-foreground">
            <Loader2 className="animate-spin" /> 正在解析…
          </div>
        )}

        {!parsing && preview.length > 0 && (
          <>
            <div className="flex items-center justify-between text-sm">
              <span className="text-muted-foreground">
                共 {preview.length} 个账号，已选 {selected.size} 个
              </span>
              <button type="button" className="cursor-pointer text-primary hover:underline" onClick={toggleAll}>
                {allSelected ? "取消全选" : "全选"}
              </button>
            </div>
            <div className="max-h-56 space-y-1 overflow-y-auto pr-1">
              {preview.map((a) => (
                <label
                  key={a.index}
                  className="flex cursor-pointer items-center gap-3 rounded-md border px-3 py-2 hover:bg-accent/50"
                >
                  <input
                    type="checkbox"
                    className="size-4 accent-primary"
                    checked={selected.has(a.index)}
                    onChange={() => toggle(a.index)}
                  />
                  <span className="min-w-0 flex-1 truncate text-sm">{previewLabel(a)}</span>
                  {a.encrypted ? (
                    <Badge variant="secondary">加密凭据</Badge>
                  ) : (
                    !a.hasToken && <Badge variant="outline">缺少 token</Badge>
                  )}
                </label>
              ))}
            </div>
          </>
        )}

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={doImport} disabled={busy || parsing || selected.size === 0}>
            {busy ? "导入中…" : "导入勾选账号"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
