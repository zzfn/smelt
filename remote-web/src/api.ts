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
 * 改共享 PTY 尺寸（会触发 SIGWINCH，PC/手机画面一起重排）。
 * 移动端镜像模式**不要**调用：手机只做本地缩放，保持与 PC 内容一致、几何解耦。
 * 保留给将来「主动接管尺寸」之类能力。
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
