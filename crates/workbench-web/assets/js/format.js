// Japanese labels and small formatters. Pure.

export const THREAD_STATE = {
  idle: "待機中",
  running: "実行中",
  waiting_for_user: "回答待ち",
  waiting_for_approval: "承認待ち",
  failed: "失敗",
};

export const RUN_PHASE = {
  pending: "準備中",
  running: "実行中",
  waiting_for_user: "回答待ち",
  waiting_for_approval: "承認待ち",
  done: "完了",
  failed: "失敗",
  cancelled: "中止",
};

export const RUN_ROLE = {
  orchestrator: "Orchestrator",
  worker: "Worker",
  planner: "Planner",
  reviewer: "Reviewer",
  advisor: "Advisor",
  other: "その他",
};

export const MESSAGE_ROLE = {
  user: "あなた",
  assistant: "エージェント",
  tool: "ツール",
};

export const RECEIPT_STATE = {
  accepted: "受付済み",
  withdrawn: "取り下げ済み",
  delivered: "配信済み",
  dropped: "未配信（run が終了）",
  unknown: "配信状況不明",
};

export const SURFACE = {
  desktop: "デスクトップ",
  web: "Web",
  cli: "CLI",
  none: "エージェント",
};

export function label(table, key) {
  return table[key] ?? String(key ?? "");
}

/** "3分前" style text for a Unix-millisecond decimal. */
export function relativeTime(millis, now = Date.now()) {
  const at = Number(millis);
  if (!Number.isFinite(at) || at <= 0) return "";
  const seconds = Math.round((at - now) / 1000);
  const abs = Math.abs(seconds);
  const format = new Intl.RelativeTimeFormat("ja", { numeric: "auto" });
  if (abs < 60) return format.format(seconds, "second");
  if (abs < 3600) return format.format(Math.round(seconds / 60), "minute");
  if (abs < 86400) return format.format(Math.round(seconds / 3600), "hour");
  if (abs < 86400 * 30) return format.format(Math.round(seconds / 86400), "day");
  return new Date(at).toLocaleDateString("ja");
}

export function absoluteTime(millis) {
  const at = Number(millis);
  if (!Number.isFinite(at) || at <= 0) return "";
  return new Date(at).toLocaleString("ja");
}

/** Last part of an id, enough to tell runs apart on screen. */
export function shortId(id) {
  const text = String(id ?? "");
  return text.length <= 10 ? text : `…${text.slice(-8)}`;
}

/** The build fields that change when another core binary starts. */
export function buildKey(hello) {
  const b = hello.server_build ?? {};
  return `${b.name ?? ""}/${b.version ?? ""}/${b.revision ?? ""}`;
}

/**
 * Whether the core behind the page is no longer the one the page was loaded
 * against: another build, another core identity or a new instance epoch.
 */
export function coreChanged(first, now) {
  if (!first || !now) return false;
  return (
    buildKey(first) !== buildKey(now) ||
    first.core !== now.core ||
    first.instance_epoch !== now.instance_epoch
  );
}
