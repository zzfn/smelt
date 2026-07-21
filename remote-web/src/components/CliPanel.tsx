import { useCallback, useEffect, useMemo, useState } from "preact/hooks";
import {
  fetchMenu,
  postAction,
  postInput,
  SessionState,
  wsUrl,
  type PermissionMenu,
} from "../api";
import { ChoiceSheet } from "./ChoiceSheet";
import { Composer } from "./Composer";
import { StatusBadge } from "./StatusBadge";
import { XtermSurface } from "./XtermSurface";
import { useTransport } from "../transport/TransportContext";

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
  const transport = useTransport();
  const canWrite =
    transport.mode === "rtc" && transport.rtc
      ? transport.rtc.writeEnabled() && writeEnabled
      : writeEnabled;

  useEffect(() => {
    // RTC 路径暂无独立 state-stream（状态靠列表刷新）；HTTP 仍走 WS
    if (transport.mode === "rtc") return;
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
  }, [sessionId, transport.mode]);

  // 菜单不在前端解析——解析器只有 Rust 那一份（crates/smelt-core/src/permission_menu.rs，GUI 与 smeltd
  // 共用），这里拉守护现场解析的结果。画面何时变只有本端最清楚（它在渲染 xterm），
  // 所以由本端 debounce 后拉一次；服务端因此不必在 PTY 泵那条每字节都过的热路径上
  // 挂解析，也不受「state 广播只由 hook 驱动、没接 hook 的 agent 永不广播」所限。
  const [menu, setMenu] = useState<PermissionMenu | null>(null);
  useEffect(() => {
    let alive = true;
    const timer = window.setTimeout(() => {
      void fetchMenu(sessionId).then((m) => {
        if (alive) setMenu(m);
      });
    }, 250);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
    // bufferText/phase 一变就重排这个 timer：画面还在刷时不断推迟，稳定 250ms 才拉。
  }, [sessionId, bufferText, state.phase]);

  // 菜单身份：标题+选项标签，变了才自动再弹出
  const menuKey = useMemo(() => {
    if (!menu) return "";
    return `${menu.summary || ""}|${menu.options.map((o) => o.label).join(";")}`;
  }, [menu]);

  useEffect(() => {
    if (menuKey) setSheetDismissed(false);
  }, [menuKey]);

  const showChoiceSheet =
    canWrite &&
    !sheetDismissed &&
    !!menu &&
    menu.options.length >= 2 &&
    // 思考中一般不是选菜单；等用户时更可信；终端已画出菜单也可弹
    // menu 非空即代表守护此刻在屏幕上真扫到了菜单，本身就是最强证据
    (state.phase === "waiting_for_user" ||
      state.phase === "awaiting_approval" ||
      state.phase === "idle");

  const sendRaw = useCallback(
    async (data: string, okMsg: string) => {
      setPending(true);
      setStatus({ text: "发送中…" });
      try {
        if (transport.mode === "rtc" && transport.rtc) {
          transport.rtc.postInput(sessionId, data);
          setStatus({ text: okMsg, kind: "ok" });
          setSheetDismissed(true);
        } else {
          const r = await postInput(sessionId, data);
          if (r.ok) {
            setStatus({ text: okMsg, kind: "ok" });
            setSheetDismissed(true);
          } else {
            setStatus({ text: r.err || "失败", kind: "err" });
          }
        }
      } catch (e) {
        setStatus({ text: e instanceof Error ? e.message : "网络问题", kind: "err" });
      } finally {
        setPending(false);
      }
    },
    [sessionId, transport.mode, transport.rtc],
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
      if (transport.mode === "rtc" && transport.rtc) {
        transport.rtc.postInput(sessionId, data);
      } else {
        void postInput(sessionId, data);
      }
    },
    [sessionId, transport.mode, transport.rtc],
  );

  const onPick = useCallback(
    async (key: string) => {
      // 直接打选项自带的数字键 + Enter，与桌面端同一种选中方式。
      // 旧实现是「↑ 顶到头 ×8 再 ↓ n-1 次」模拟导航——那依赖「多按几次总能顶到头」
      // 的假设，菜单一旦有滚动或分页就会错位，且与桌面端行为不一致。
      await sendRaw(`${key}\r`, `已选 ${key}`);
    },
    [sendRaw],
  );

  const actionable =
    canWrite &&
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
            <span class="text-accent/90">手机布局 · 滚轮同步</span>
          </p>
        </div>
        <StatusBadge phase={state.phase} />
      </header>

      {!canWrite && (
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
              if (transport.mode === "rtc" && transport.rtc) {
                transport.rtc.postAction(sessionId, "approve");
                setStatus({ text: "已批准", kind: "ok" });
              } else {
                const r = await postAction(sessionId, "approve");
                setStatus(
                  r.ok ? { text: "已批准", kind: "ok" } : { text: r.err || "失败", kind: "err" },
                );
              }
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
              if (transport.mode === "rtc" && transport.rtc) {
                transport.rtc.postAction(sessionId, "deny");
                setStatus({ text: "已拒绝", kind: "ok" });
              } else {
                const r = await postAction(sessionId, "deny");
                setStatus(
                  r.ok ? { text: "已拒绝", kind: "ok" } : { text: r.err || "失败", kind: "err" },
                );
              }
              setPending(false);
            }}
          >
            拒绝
          </button>
        </div>
      ) : null}

      {menu && canWrite && sheetDismissed ? (
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
          writeEnabled={canWrite}
          onUserData={canWrite ? onTermData : undefined}
          onBufferText={setBufferText}
          class="h-full"
        />
      </div>

      {canWrite ? (
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
