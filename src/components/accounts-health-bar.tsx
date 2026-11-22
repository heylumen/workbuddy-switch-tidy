import { useMemo, useState } from "react";
import { AlertTriangle, ChevronDown } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { displayName } from "@/lib/account-display";
import type { AccountMeta } from "@/lib/types";

interface Issue {
  id: string;
  name: string;
  reasons: string[];
  severity: "error" | "warning";
}

const DAY_MS = 24 * 60 * 60 * 1000;
const EXPIRING_SOON_DAYS = 7;

function toNumber(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim()) {
    const parsed = Number(value);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

/**
 * 归一为毫秒：小于 1e10 视为秒级时间戳。
 * 注：这会把 1970-04 之前的"毫秒"值误判为秒；本项目令牌有效期均为近年时间，可接受。
 */
function toMillis(value: unknown): number | null {
  const raw = toNumber(value);
  if (raw === null || raw <= 0) return null;
  return raw < 10_000_000_000 ? raw * 1000 : raw;
}

/**
 * 账号异常聚合：把散落在卡片上的登录态问题汇总成一条可操作的提示。
 *
 * 判据全部来自 `account_meta` 已有字段，不额外请求后端：
 * 需要重新登录 / 访问令牌过期 / 刷新令牌临期（7 天内）/ 上次刷新失败 / 缺少有效期。
 */
export function AccountsHealthBar({
  accounts,
  onFocus,
}: {
  accounts: AccountMeta[];
  onFocus: (accountId: string) => void;
}) {
  const [open, setOpen] = useState(false);

  const issues = useMemo<Issue[]>(() => {
    const now = Date.now();
    const rows: Issue[] = [];
    for (const account of accounts) {
      const id = account.id ?? "";
      if (!id) continue;
      const reasons: string[] = [];
      let severity: "error" | "warning" = "warning";

      if (account.needsRelogin) {
        reasons.push("需要重新登录");
        severity = "error";
      }
      const expiresAt = toMillis(account.expiresAt);
      const refreshExpiresAt = toMillis(account.refreshExpiresAt);
      if (expiresAt !== null && expiresAt <= now) {
        reasons.push("访问令牌已过期");
        severity = "error";
      } else if (refreshExpiresAt !== null && refreshExpiresAt - now <= EXPIRING_SOON_DAYS * DAY_MS) {
        const days = Math.max(1, Math.ceil((refreshExpiresAt - now) / DAY_MS));
        reasons.push(`刷新令牌 ${days} 天内到期`);
      }
      if (account.last_refresh_error) {
        reasons.push("上次刷新失败");
      }
      if (expiresAt === null && refreshExpiresAt === null) {
        reasons.push("缺少令牌有效期信息");
      }
      if (reasons.length > 0) {
        rows.push({ id, name: displayName(account), reasons, severity });
      }
    }
    return rows;
  }, [accounts]);

  if (issues.length === 0) return null;

  const errors = issues.filter((item) => item.severity === "error").length;
  const warnings = issues.length - errors;
  const tone = errors > 0
    ? "border-destructive/30 bg-destructive/5"
    : "border-amber-500/30 bg-amber-500/5";

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <div className={`mb-4 rounded-xl border px-3 py-2 ${tone}`}>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2 text-sm">
            <AlertTriangle className="size-4 shrink-0" />
            <span className="font-medium">{issues.length} 个账号需要注意</span>
            <span className="text-xs text-muted-foreground">
              {errors > 0 ? `${errors} 个失效` : ""}
              {errors > 0 && warnings > 0 ? " · " : ""}
              {warnings > 0 ? `${warnings} 个临期/异常` : ""}
            </span>
          </div>
          <CollapsibleTrigger asChild>
            <Button variant="ghost" size="sm" className="h-7 gap-1 text-xs">
              {open ? "收起" : "查看"}
              <ChevronDown className={`size-3.5 transition-transform ${open ? "rotate-180" : ""}`} />
            </Button>
          </CollapsibleTrigger>
        </div>
        <CollapsibleContent>
          <ul className="mt-2 space-y-1">
            {issues.map((issue) => (
              <li key={issue.id} className="flex flex-wrap items-center justify-between gap-2 rounded-md bg-background/60 px-2 py-1 text-xs">
                <span className="flex min-w-0 flex-wrap items-center gap-1.5">
                  <span className="font-medium">{issue.name}</span>
                  {issue.reasons.map((reason) => (
                    <Badge
                      key={reason}
                      variant={issue.severity === "error" ? "destructive" : "warning"}
                      className="text-[10px]"
                    >
                      {reason}
                    </Badge>
                  ))}
                </span>
                <Button variant="outline" size="sm" className="h-6 px-2 text-[11px]" onClick={() => onFocus(issue.id)}>
                  定位
                </Button>
              </li>
            ))}
          </ul>
          <p className="mt-1.5 text-[11px] text-muted-foreground">
            建议先处理「需要重新登录 / 访问令牌已过期」的账号：切号前预检会直接拒绝它们。
          </p>
        </CollapsibleContent>
      </div>
    </Collapsible>
  );
}
