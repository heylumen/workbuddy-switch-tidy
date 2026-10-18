import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { AccountCard } from "./account-card";
import type { AccountMeta, CreditExpiry, CreditResource } from "@/lib/types";

/**
 * 企业版卡片展示（issue #167）：
 * - `isEnterprise` 渲染「企业版」徽标，个人版账号不渲染；
 * - `unlimited` 渲染「不限量」+「企业配额」，不出现「暂无可用积分」空态；
 * - 个人版有限配额展示不回归。
 *
 * 组件无 DOM 依赖地走 `renderToStaticMarkup`（与现有 Vitest node 环境一致，
 * 不引入 jsdom / testing-library）。
 */

function account(overrides: Partial<AccountMeta> = {}): AccountMeta {
  return {
    id: "acc-1",
    uid: "u-1",
    email: "user@example.com",
    nickname: "测试账号",
    enterpriseName: null,
    expiresAt: null,
    refreshExpiresAt: null,
    refreshedAt: null,
    createdAt: null,
    needsRelogin: false,
    needsReloginReason: null,
    ...overrides,
  };
}

function resource(overrides: Partial<CreditResource> = {}): CreditResource {
  return {
    packageCode: "demo-package",
    packageName: "CodeBuddy 个人版积分包",
    total: 2000,
    remaining: 1500,
    used: 500,
    status: 1,
    expireAt: Date.now() + 30 * 24 * 3600 * 1000,
    expired: false,
    expiringSoon: false,
    ...overrides,
  };
}

function render(accountMeta: AccountMeta, credit: CreditExpiry, compact = false): string {
  return renderToStaticMarkup(
    createElement(AccountCard, { account: accountMeta, onDelete: () => {}, credit, compact }),
  );
}

/** 截取 `<header>` 段，用于断言徽标是否出现在卡片头部。 */
function headerOf(html: string): string {
  return html.slice(html.indexOf("<header"), html.indexOf("</header>"));
}

describe("企业版账号卡片", () => {
  it("企业版有限配额：渲染「企业版」徽标与单条「企业配额」", () => {
    const html = render(
      account({ isEnterprise: true }),
      {
        ok: true,
        unlimited: false,
        totalCapacity: 2000,
        totalRemaining: 1500,
        expiringSoonRemaining: 0,
        expiredRemaining: 0,
        soonestExpireAt: Date.now() + 30 * 24 * 3600 * 1000,
        expiringSoon: false,
        expired: false,
        resources: [resource({ packageName: "企业配额" })],
      },
    );

    expect(html).toContain("企业版");
    expect(html).toContain("企业配额");
    expect(html).toContain("1,500");
    expect(html).not.toContain("暂无可用积分");
    expect(html).not.toContain("不限量");
  });

  it("企业版不限量：渲染「不限量」，不出现「暂无可用积分」", () => {
    const html = render(
      account({ isEnterprise: true }),
      {
        ok: true,
        unlimited: true,
        totalCapacity: null,
        totalRemaining: null,
        expiringSoonRemaining: 0,
        expiredRemaining: 0,
        soonestExpireAt: null,
        expiringSoon: false,
        expired: false,
        resources: [],
      },
    );

    expect(html).toContain("不限量");
    expect(html).toContain("企业配额");
    expect(html).toContain("企业配额 · 不限量");
    expect(html).not.toContain("暂无可用积分");
    expect(html).not.toContain("0 个积分包");
  });

  it("企业账号查询失败：渲染可读错误，不出现「暂无可用积分」", () => {
    const html = render(account({ isEnterprise: true }), {
      ok: false,
      error: "企业账号缺少企业 ID，无法查询企业配额",
    });

    expect(html).toContain("企业版");
    expect(html).toContain("企业账号缺少企业 ID，无法查询企业配额");
    expect(html).not.toContain("暂无可用积分");
    expect(html).not.toContain("不限量");
  });

  it("个人版账号不回归：无「企业版」徽标，正常展示积分包与余额", () => {
    const html = render(account(), {
      ok: true,
      totalCapacity: 2000,
      totalRemaining: 1500,
      expiringSoonRemaining: 0,
      expiredRemaining: 0,
      soonestExpireAt: Date.now() + 30 * 24 * 3600 * 1000,
      expiringSoon: false,
      expired: false,
      resources: [resource()],
    });

    expect(html).not.toContain("企业版");
    expect(html).not.toContain("不限量");
    expect(html).toContain("1 个积分包");
    expect(html).toContain("1,500");
  });

  it("宽松卡片：企业版徽标在头部 chips 中", () => {
    const html = render(account({ isEnterprise: true }), {
      ok: true,
      unlimited: false,
      totalCapacity: 2000,
      totalRemaining: 1500,
      expiringSoonRemaining: 0,
      expiredRemaining: 0,
      soonestExpireAt: null,
      expiringSoon: false,
      expired: false,
      resources: [resource({ packageName: "企业配额" })],
    });

    expect(headerOf(html)).toContain("企业版");
  });

  it("紧凑卡片：企业版徽标在积分区，头部不出现（避免挤压账号名）", () => {
    const html = render(
      account({ isEnterprise: true }),
      {
        ok: true,
        unlimited: false,
        totalCapacity: 2000,
        totalRemaining: 1500,
        expiringSoonRemaining: 0,
        expiredRemaining: 0,
        soonestExpireAt: null,
        expiringSoon: false,
        expired: false,
        resources: [resource({ packageName: "企业配额" })],
      },
      true,
    );

    // 头部单行空间已被状态 chip 与操作按钮占满：徽标不得出现在 header 内。
    expect(headerOf(html)).not.toContain("企业版");
    // 积分区保留标注与「企业配额」语义。
    expect(html).toContain("企业版");
    expect(html).toContain("企业配额");
    expect(html).toContain("1,500");
  });
});
