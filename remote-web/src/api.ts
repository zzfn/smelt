/** 与 remote_gateway 对齐的前端 API / WS 客户端 */

export type Phase =
  | "thinking"
  | "executing_tool"
  | "awaiting_approval"
  | "waiting_for_user"
  | "idle"
  | "dead"
  | string;

export type SessionInfo = {
  id: string;
  phase: Phase;
  pending_question?: string | null;
  name: string;
  project: string;
  parent_session?: string | null;
  cwd?: string | null;
};

export type SessionState = {
  id?: string;
  phase?: Phase;
  pending_question?: string | null;
  phase_since?: number;
  title?: string | null;
  cwd?: string | null;
};

export function getToken(): string {
  const q = new URLSearchParams(location.search);
  const t = q.get("token");
  if (t) {
    sessionStorage.setItem("smelt_token", t);
    return t;
  }
  return sessionStorage.getItem("smelt_token") || "";
}

export function tokenQuery(): string {
  const t = getToken();
  return t ? `token=${encodeURIComponent(t)}` : "";
}

export function withToken(path: string): string {
  const tq = tokenQuery();
  if (!tq) return path;
  return path.includes("?") ? `${path}&${tq}` : `${path}?${tq}`;
}

export async function fetchSessions(): Promise<SessionInfo[]> {
  const resp = await fetch(withToken("/sessions"));
  if (!resp.ok) throw new Error(await resp.text());
  const data = await resp.json();
  return data.sessions ?? [];
}

/**
 * 权限菜单 —— 与 crates/smelt-core/src/permission_menu.rs 的 PermissionPrompt / PermissionOption 一一对应。
 *
 * 这里**只有类型、没有解析**：解析器是 Rust 那一份（GUI 与 smeltd 共用），前端拉现成
 * 结果来渲染。曾经这边自己写过一份 TS 解析器（lib/parseChoiceMenu.ts），与 Rust 那份
 * 同日诞生后各自演化、实测已漂移到对同一段文本给出相反结论，故删除。别再加回来。
 */
export type PermissionOptionKind = "allow" | "deny" | "other";

export type PermissionOption = {
  /** 注入 PTY 的键，通常是 "1" / "2" / "3" —— 选中就是打它 */
  key: string;
  label: string;
  kind: PermissionOptionKind;
  /** 选项下方的副文案 */
  description?: string;
  /** TUI 当前高亮在这一项（仅视觉提示；选中一律靠打 key，不依赖高亮位置） */
  active: boolean;
};

export type PermissionMenu = {
  summary?: string | null;
  options: PermissionOption[];
};

/**
 * 拉当前可视区里的权限菜单（守护现场解析）。没菜单返回 null。
 * 只读，不需要写权限——只读链接也该看得见 agent 在问什么。
 */
export async function fetchMenu(id: string): Promise<PermissionMenu | null> {
  const resp = await fetch(withToken(`/s/${encodeURIComponent(id)}/menu`));
  if (!resp.ok) return null;
  const body = await resp.json().catch(() => null);
  return body?.menu ?? null;
}

export async function postAction(
  id: string,
  kind: "approve" | "deny" | "reply",
  text?: string,
): Promise<{ ok: boolean; err?: string }> {
  const resp = await fetch(withToken(`/s/${encodeURIComponent(id)}/action`), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kind, text }),
  });
  const body = await resp.json().catch(() => ({}));
  if (!resp.ok) return { ok: false, err: body.err || resp.statusText };
  return body;
}

export async function postInput(
  id: string,
  data: string,
): Promise<{ ok: boolean; err?: string }> {
  const resp = await fetch(withToken(`/s/${encodeURIComponent(id)}/input`), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ data }),
  });
  const text = await resp.text();
  let body: { ok?: boolean; err?: string } = {};
  try {
    body = text ? JSON.parse(text) : {};
  } catch {
    return { ok: false, err: text || "空响应" };
  }
  if (!resp.ok) return { ok: false, err: body.err || resp.statusText };
  return { ok: !!body.ok, err: body.err };
}

/**
 * 改共享 PTY 尺寸（SIGWINCH；手机打开 CLI 时按视口调用，操作体验优先）。
 * 通常用户只在一端操作：以当前操作端几何为准。
 */
export async function postResize(
  id: string,
  cols: number,
  rows: number,
  cellW = 0,
  cellH = 0,
): Promise<{ ok: boolean; err?: string }> {
  const resp = await fetch(withToken(`/s/${encodeURIComponent(id)}/resize`), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      cols,
      rows,
      cell_w: cellW,
      cell_h: cellH,
    }),
  });
  const text = await resp.text();
  let body: { ok?: boolean; err?: string } = {};
  try {
    body = text ? JSON.parse(text) : {};
  } catch {
    return { ok: false, err: text || "空响应" };
  }
  if (!resp.ok) return { ok: false, err: body.err || resp.statusText };
  return { ok: !!body.ok, err: body.err };
}

export function wsUrl(path: string): string {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${withToken(path)}`;
}

export const PHASE_META: Record<string, { label: string; color: string }> = {
  thinking: { label: "思考中", color: "#3b82f6" },
  executing_tool: { label: "执行中", color: "#3b82f6" },
  awaiting_approval: { label: "等批准", color: "#ef4444" },
  waiting_for_user: { label: "等回复", color: "#f59e0b" },
  idle: { label: "空闲", color: "#52525b" },
  dead: { label: "结束", color: "#52525b" },
};

export function phaseMeta(phase?: string) {
  return PHASE_META[phase || "idle"] || { label: phase || "未知", color: "#52525b" };
}
