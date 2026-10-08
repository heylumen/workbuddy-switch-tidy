/**
 * 账号显示名：按本地 `displayField` 取字段（备注 / 手机号 / 账号名），
 * 选定字段为空时回退 `nickname → uid → id`。
 *
 * 所有展示账号名的位置统一走本函数，保证卡片、切换弹窗、选择列表等名称一致。
 */
export function displayName(account) {
    const byField = account.displayField === "phone"
        ? account.phoneNumber
        : account.displayField === "note"
            ? account.note
            : account.nickname;
    return byField || account.nickname || account.uid || account.id || "未命名账号";
}
/** 账号身份行：邮箱脱敏展示（本地段只留首字符），无邮箱时回退 UID / ID。 */
export function accountIdentity(account) {
    if (account.email) {
        const [local, domain] = account.email.split("@");
        if (!domain)
            return account.email;
        return `${local.slice(0, 1)}${"*".repeat(Math.max(3, local.length - 1))}@${domain}`;
    }
    return account.uid ? `UID · ${account.uid}` : `ID · ${account.id}`;
}
