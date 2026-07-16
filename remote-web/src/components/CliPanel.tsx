import { useCallback, useEffect, useMemo, useState } from "preact/hooks";
import { postAction, postInput, SessionState, wsUrl } from "../api";
import {
  choiceKeySequence,
  parseChoiceMenu,
  type ChoiceMenu,
} from "../lib/parseChoiceMenu";
import { ChoiceSheet } from "./ChoiceSheet";
import { Composer } from "./Composer";
import { StatusBadge } from "./StatusBadge";
import { XtermSurface } from "./XtermSurface";

type Props = {
  sessionId: string;
  name: string;
  subtitle?: string;
  writeEnabled: boolean;
  onBack: () => void;
};

/**
 * 会话 CLI 面板。
 * 选择菜单：从终端缓冲解析后用底部弹层自渲染（大按钮），不依赖 TUI 里的小光标。
 */
export function CliPanel({ sessionId, name, subtitle, writeEnabled, onBack }: Props) {
  const [state, setState] = useState<SessionState>({ phase: "idle" });
  const [status, setStatus] = useState<{ text: string; kind?: "ok" | "err" } | null>(null);
  const [pending, setPending] = useState(false);
  const [bufferText, setBufferText] = useState("");
  const [sheetDismissed, setSheetDismissed] = useState(false);

  useEffect(() => {
    let retry = 1000;
    let closed = false;
    let ws: WebSocket | null = null;
    const connect = () => {
      if (closed) return;
      ws = new WebSocket(wsUrl(`/s/${encodeURIComponent(sessionId)}/state-stream`));
      ws.onopen = () => {
        retry = 1000;
      };
      ws.onmessage = (ev) => {
        try {
          setState(JSON.parse(ev.data));
        } catch {
          /* ignore */
        }
      };
      ws.onclose = () => {
        if (closed) return;
        setTimeout(connect, retry);
        retry = Math.min(retry * 2, 8000);
      };
      ws.onerror = () => ws?.close();
    };
    connect();
    return () => {
      closed = true;
      ws?.close();
    };
  }, [sessionId]);

  const menuFromTerm = useMemo(() => parseChoiceMenu(bufferText), [bufferText]);
  const menuFromQuestion = useMemo(() => {
    const q = state.pending_question?.trim();
    if (!q) return null;
    return parseChoiceMenu(q);
  }, [state.pending_question]);

  const menu: ChoiceMenu | null = menuFromTerm || menuFromQuestion;
  // 菜单身份：标题+选项标签，变了才自动再弹出
  const menuKey = useMemo(() => {
    if (!menu) return "";
    return `${menu.title || ""}|${menu.prompt || ""}|${menu.options.map((o) => o.label).join(";")}`;
  }, [menu]);

  useEffect(() => {
    if (menuKey) setSheetDismissed(false);
  }, [menuKey]);

  const showChoiceSheet =
    writeEnabled &&
    !sheetDismissed &&
    !!menu &&
    menu.options.length >= 2 &&
    // 思考中一般不是选菜单；等用户时更可信；终端已画出菜单也可弹
    (state.phase === "waiting_for_user" ||
      state.phase === "awaiting_approval" ||
      state.phase === "idle" ||
      !!menuFromTerm);

  const sendRaw = useCallback(
    async (data: string, okMsg: string) => {
      setPending(true);
      setStatus({ text: "发送中…" });
      try {
        const r = await postInput(sessionId, data);
        if (r.ok) {
          setStatus({ text: okMsg, kind: "ok" });
          setSheetDismissed(true);
        } else {
          setStatus({ text: r.err || "失败", kind: "err" });
        }
      } catch (e) {
        setStatus({ text: e instanceof Error ? e.message : "网络问题", kind: "err" });
      } finally {
        setPending(false);
      }
    },
    [sessionId],
  );

  const sendText = useCallback(
    async (text: string) => {
      const data = text.endsWith("\n") || text.endsWith("\r") ? text : `${text}\r`;
      await sendRaw(data, "已发送");
    },
    [sendRaw],
  );

  const onTermData = useCallback(
    (data: string) => {
      void postInput(sessionId, data);
    },
    [sessionId],
  );

  const onPick = useCallback(
    async (index: number) => {
      if (!menu) return;
      // 方向键导航到目标项 + Enter（用最大序号，避免有缺口时 options.length 算错）
      const maxIdx = Math.max(...menu.options.map((o) => o.index), index);
      const seq = choiceKeySequence(index, maxIdx);
      await sendRaw(seq, `已选 ${index}`);
    },
    [menu, sendRaw],
  );

  const actionable =
    writeEnabled &&
    !showChoiceSheet &&
    (state.phase === "awaiting_approval" || state.phase === "waiting_for_user");
  const question = (state.pending_question || "").trim();

  return (
    <div class="flex h-full max-w-lg mx-auto flex-col overflow-hidden bg-bg">
      <header class="grid shrink-0 grid-cols-[2rem_1fr_auto] items-center gap-1 border-b border-border bg-panel px-2 py-1.5">
        <button
          type="button"
          class="flex h-8 w-8 items-center justify-center rounded-lg text-lg text-accent active:bg-card"
          onClick={onBack}
          aria-label="返回列表"
        >
          ←
        </button>
        <div class="min-w-0 text-center">
          <h1 class="truncate text-sm font-semibold leading-tight">{name}</h1>
          <p class="truncate text-[10px] text-muted">
            {subtitle ? `${subtitle} · ` : ""}
            <span class="text-accent/90">镜像独立</span>
          </p>
        </div>
        <StatusBadge phase={state.phase} />
      </header>

      {!writeEnabled && (
        <div class="mx-2 mt-1.5 shrink-0 rounded-lg border border-[#243044] bg-[#151a24] px-2.5 py-1.5 text-[11px] leading-snug text-[#9aa8bc]">
          只读观战。写入请在 Mac 打开「允许远程写入」。
        </div>
      )}

      {/* 有选择弹层时，问题改在弹层里展示，避免占高度 */}
      {question && !showChoiceSheet ? (
        <div class="mx-2 mt-1.5 shrink-0 rounded-lg border border-border bg-card px-2.5 py-2">
          <div class="mb-0.5 text-[10px] text-muted">正在问你</div>
          <p class="max-h-20 overflow-y-auto whitespace-pre-wrap text-xs leading-snug">
            {question}
          </p>
        </div>
      ) : null}

      {actionable ? (
        <div class="mx-2 mt-1.5 flex shrink-0 gap-2">
          <button
            type="button"
            class="flex-1 rounded-lg bg-ok py-2 text-sm font-semibold text-white active:scale-[0.98]"
            disabled={pending}
            onClick={async () => {
              setPending(true);
              const r = await postAction(sessionId, "approve");
              setStatus(
                r.ok ? { text: "已批准", kind: "ok" } : { text: r.err || "失败", kind: "err" },
              );
              setPending(false);
            }}
          >
            批准
          </button>
          <button
            type="button"
            class="flex-1 rounded-lg bg-danger py-2 text-sm font-semibold text-white active:scale-[0.98]"
            disabled={pending}
            onClick={async () => {
              setPending(true);
              const r = await postAction(sessionId, "deny");
              setStatus(
                r.ok ? { text: "已拒绝", kind: "ok" } : { text: r.err || "失败", kind: "err" },
              );
              setPending(false);
            }}
          >
            拒绝
          </button>
        </div>
      ) : null}

      {menu && writeEnabled && sheetDismissed ? (
        <button
          type="button"
          class="mx-2 mt-1.5 shrink-0 rounded-lg border border-accent/40 bg-accent/10 py-2 text-sm font-medium text-accent"
          onClick={() => setSheetDismissed(false)}
        >
          打开选择面板（{menu.options.length} 项）
        </button>
      ) : null}

      {status ? (
        <p
          class={`mx-2.5 mt-1 shrink-0 text-[11px] ${
            status.kind === "ok"
              ? "text-ok"
              : status.kind === "err"
                ? "text-danger"
                : "text-muted"
          }`}
        >
          {status.text}
        </p>
      ) : null}

      <div class="relative mx-1.5 mt-1.5 min-h-0 flex-1 overflow-hidden rounded-t-xl border border-b-0 border-border bg-[#08080a]">
        <XtermSurface
          sessionId={sessionId}
          writeEnabled={writeEnabled}
          onUserData={writeEnabled ? onTermData : undefined}
          onBufferText={setBufferText}
          class="h-full"
        />
      </div>

      {writeEnabled ? (
        <div class="mx-1.5 mb-[max(0.35rem,env(safe-area-inset-bottom))] shrink-0 overflow-hidden rounded-b-xl border border-border">
          <Composer
            disabled={showChoiceSheet}
            pending={pending}
            onSend={sendText}
            placeholder={showChoiceSheet ? "请在上方弹层中选择…" : "输入命令或回复…"}
          />
        </div>
      ) : (
        <div class="mx-1.5 mb-2 h-1.5 shrink-0 rounded-b-xl border border-t-0 border-border bg-panel" />
      )}

      {showChoiceSheet && menu ? (
        <ChoiceSheet
          menu={menu}
          busy={pending}
          onSelect={onPick}
          onCancel={() => setSheetDismissed(true)}
          onCustom={(text) => void sendText(text)}
        />
      ) : null}
    </div>
  );
}
