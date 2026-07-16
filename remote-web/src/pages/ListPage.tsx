import { useEffect, useMemo, useState } from "preact/hooks";
import { fetchSessions, SessionInfo } from "../api";
import { StatusBadge } from "../components/StatusBadge";

const WAITING = new Set(["awaiting_approval", "waiting_for_user"]);

type Props = {
  onOpen: (s: SessionInfo) => void;
};

export function ListPage({ onOpen }: Props) {
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [onlyWaiting, setOnlyWaiting] = useState(false);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let alive = true;
    const load = () =>
      fetchSessions()
        .then((s) => {
          if (alive) {
            setSessions(s);
            setErr(null);
          }
        })
        .catch((e) => {
          if (alive) setErr(e instanceof Error ? e.message : String(e));
        })
        .finally(() => {
          if (alive) setLoading(false);
        });
    load();
    const t = setInterval(load, 4000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  const filtered = useMemo(
    () => (onlyWaiting ? sessions.filter((s) => WAITING.has(s.phase)) : sessions),
    [sessions, onlyWaiting],
  );

  // project → parent? → items，保持服务端顺序
  const tree = useMemo(() => {
    type Row =
      | { kind: "leaf"; s: SessionInfo }
      | { kind: "group"; parent: string; kids: SessionInfo[] };
    const projects: { name: string; rows: Row[] }[] = [];
    const projectOrder: string[] = [];

    for (const s of filtered) {
      if (!projectOrder.includes(s.project)) {
        projectOrder.push(s.project);
        projects.push({ name: s.project, rows: [] });
      }
      const proj = projects.find((p) => p.name === s.project)!;
      if (!s.parent_session) {
        proj.rows.push({ kind: "leaf", s });
      } else {
        const g = proj.rows.find(
          (r) => r.kind === "group" && r.parent === s.parent_session,
        ) as { kind: "group"; parent: string; kids: SessionInfo[] } | undefined;
        if (g) g.kids.push(s);
        else proj.rows.push({ kind: "group", parent: s.parent_session, kids: [s] });
      }
    }
    return projects;
  }, [filtered]);

  return (
    <div class="mx-auto min-h-full max-w-lg bg-bg px-4 pb-10 pt-5">
      <h1 class="mb-3 text-lg font-semibold">会话</h1>
      <label class="mb-4 flex cursor-pointer items-center gap-2 text-sm text-muted">
        <input
          type="checkbox"
          checked={onlyWaiting}
          onChange={(e) => setOnlyWaiting((e.target as HTMLInputElement).checked)}
        />
        只看等你处理的
      </label>

      {loading && <p class="text-sm text-muted">加载中…</p>}
      {err && <p class="text-sm text-danger">{err}</p>}
      {!loading && !err && filtered.length === 0 && (
        <p class="text-sm text-muted">目前没有会话</p>
      )}

      {tree.map((proj) => (
        <section key={proj.name} class="mb-5">
          <div class="mb-1.5 px-0.5 text-xs font-semibold text-muted">
            📁 {proj.name}
          </div>
          <ul class="space-y-1.5">
            {proj.rows.map((row) =>
              row.kind === "leaf" ? (
                <SessionRow key={row.s.id} s={row.s} onOpen={onOpen} />
              ) : (
                <li
                  key={row.parent}
                  class="rounded-xl border border-border bg-card/80 px-2 py-1.5"
                >
                  <div class="px-1 pb-1 text-[13px] text-muted">⊞ {row.parent}</div>
                  <ul class="ml-2 space-y-1 border-l border-border pl-2">
                    {row.kids.map((s) => (
                      <SessionRow key={s.id} s={s} onOpen={onOpen} nested />
                    ))}
                  </ul>
                </li>
              ),
            )}
          </ul>
        </section>
      ))}

      <p class="mt-6 text-xs leading-relaxed text-muted/70">
        名称与分组来自本机工作台侧栏。点进会话使用 CLI 面板；复杂 TUI 由 xterm 渲染。
      </p>
    </div>
  );
}

function SessionRow({
  s,
  onOpen,
  nested,
}: {
  s: SessionInfo;
  onOpen: (s: SessionInfo) => void;
  nested?: boolean;
}) {
  return (
    <li>
      <button
        type="button"
        class={`flex w-full items-center gap-2 rounded-xl border border-border bg-card px-3 py-2.5 text-left active:bg-[#1c1c1f] ${
          nested ? "bg-[#141416]" : ""
        }`}
        onClick={() => onOpen(s)}
      >
        <span
          class="h-2 w-2 shrink-0 rounded-full"
          style={{ background: phaseDot(s.phase) }}
        />
        <span class="min-w-0 flex-1 truncate text-[15px] font-medium">{s.name}</span>
        <StatusBadge phase={s.phase} />
      </button>
      {s.pending_question ? (
        <p class="mt-1 line-clamp-2 px-3 text-xs text-muted">{s.pending_question}</p>
      ) : null}
    </li>
  );
}

function phaseDot(phase: string) {
  if (phase === "awaiting_approval") return "#ef4444";
  if (phase === "waiting_for_user") return "#f59e0b";
  if (phase === "thinking" || phase === "executing_tool") return "#3b82f6";
  return "#555";
}
