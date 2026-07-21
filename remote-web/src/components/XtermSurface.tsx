import { useEffect, useRef } from "preact/hooks";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { postInput, postResize, wsUrl } from "../api";
import { serializeVisibleScreen } from "../lib/serializeScreen";
import { useTransport } from "../transport/TransportContext";

type Props = {
  sessionId: string;
  /** 保留接口兼容；移动端不把键盘打进 xterm（只用自建 Composer） */
  onUserData?: (data: string) => void;
  writeEnabled?: boolean;
  class?: string;
  onBufferText?: (text: string) => void;
};

type CoreDims = {
  _renderService?: {
    dimensions?: {
      css?: { cell?: { width: number; height: number } };
    };
  };
};

const THEME = {
  background: "#08080a",
  foreground: "#d4d4d8",
  cursor: "#5b9fd4",
  cursorAccent: "#08080a",
  selectionBackground: "#3a557066",
  black: "#18181b",
  red: "#f87171",
  green: "#4ade80",
  yellow: "#fbbf24",
  blue: "#60a5fa",
  magenta: "#c084fc",
  cyan: "#22d3ee",
  white: "#e4e4e7",
  brightBlack: "#52525b",
  brightRed: "#fca5a5",
  brightGreen: "#86efac",
  brightYellow: "#fde68a",
  brightBlue: "#93c5fd",
  brightMagenta: "#d8b4fe",
  brightCyan: "#67e8f9",
  brightWhite: "#fafafa",
} as const;

const FONT = 13;
const FONT_FAMILY =
  'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Monaco, Consolas, monospace';

/**
 * 移动端终端面 —— 操作优先
 *
 * | 维度 | 策略 |
 * |------|------|
 * | 尺寸 | 手机 fit → postResize |
 * | 输入 | **仅**自建 Composer（xterm 永久 disableStdin，不聚焦 Claude TUI 输入框） |
 * | 滚动 | 同步：发 **SGR 鼠标滚轮**（与桌面 smelt 一致），**不发 ↑↓** |
 * |      | Claude 会把方向键当成输入历史；故黄字 “Scroll wheel is sending arrow keys” |
 */
export function XtermSurface({
  sessionId,
  writeEnabled,
  class: cls,
  onBufferText,
}: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const onBufRef = useRef(onBufferText);
  onBufRef.current = onBufferText;
  const transport = useTransport();

  useEffect(() => {
    const host = hostRef.current;
    const wrap = wrapRef.current;
    if (!host || !wrap) return;

    // 永远不接收键盘：文字只走底部 Composer，避免「聚焦 Claude 输入框」后
    // 滑动发的键被当成改 prompt / 历史。
    const term = new Terminal({
      convertEol: false,
      cursorBlink: false,
      disableStdin: true,
      fontSize: FONT,
      lineHeight: 1.15,
      letterSpacing: 0,
      fontFamily: FONT_FAMILY,
      theme: THEME,
      scrollback: 8000,
      allowTransparency: false,
      windowsMode: false,
      macOptionIsMeta: true,
      scrollOnUserInput: false,
      rightClickSelectsWord: false,
    });

    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(host);

    // 彻底禁止 xterm 隐藏 textarea 抢焦点
    const lockTermFocus = () => {
      const ta = host.querySelector("textarea.xterm-helper-textarea") as HTMLTextAreaElement | null;
      if (!ta) return;
      ta.setAttribute("readonly", "true");
      ta.setAttribute("tabindex", "-1");
      ta.setAttribute("aria-hidden", "true");
      ta.blur();
      const refocus = () => {
        ta.blur();
      };
      ta.addEventListener("focus", refocus);
      return () => ta.removeEventListener("focus", refocus);
    };
    let unlockFocus = lockTermFocus();
    // open 后 DOM 可能稍后才有 textarea
    requestAnimationFrame(() => {
      unlockFocus?.();
      unlockFocus = lockTermFocus();
    });

    const core = term as unknown as { _core?: CoreDims };
    const getCellSize = () => {
      const cell = core._core?._renderService?.dimensions?.css?.cell;
      if (cell && cell.width > 0 && cell.height > 0) {
        return { w: cell.width, h: cell.height };
      }
      return { w: FONT * 0.6, h: FONT * 1.2 };
    };

    let lastCols = 0;
    let lastRows = 0;
    let resizeTimer: number | null = null;
    let resizeTimer2: number | null = null;

    const pushPtySize = () => {
      const cols = term.cols;
      const rows = term.rows;
      if (cols === lastCols && rows === lastRows) return;
      lastCols = cols;
      lastRows = rows;
      const { w: cellW, h: cellH } = getCellSize();
      if (resizeTimer != null) window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(() => {
        resizeTimer = null;
        const cw = Math.max(1, Math.round(cellW));
        const ch = Math.max(1, Math.round(cellH));
        if (transport.mode === "rtc" && transport.rtc) {
          transport.rtc.postResize(sessionId, cols, rows, cw, ch);
        } else {
          void postResize(sessionId, cols, rows, cw, ch);
        }
      }, 120);
    };

    const layoutPhone = () => {
      if (host.clientWidth < 40 || host.clientHeight < 40) return;
      term.options.fontSize = FONT;
      try {
        fitAddon.fit();
      } catch {
        /* ignore */
      }
      const { w: cellW, h: cellH } = getCellSize();
      const availW = Math.max(host.clientWidth - 8, 60);
      const availH = Math.max(host.clientHeight - 4, 40);
      const cols = Math.max(20, Math.min(120, Math.floor(availW / Math.max(cellW, 1)) - 1));
      const rows = Math.max(10, Math.min(80, Math.floor(availH / Math.max(cellH, 1))));
      if (cols !== term.cols || rows !== term.rows) {
        try {
          term.resize(cols, rows);
        } catch {
          /* ignore */
        }
      }
      const screen = host.querySelector(".xterm-screen") as HTMLElement | null;
      if (screen) {
        let guard = 0;
        while (screen.offsetWidth > host.clientWidth - 1 && term.cols > 24 && guard < 8) {
          try {
            term.resize(term.cols - 1, term.rows);
          } catch {
            break;
          }
          guard++;
        }
      }
      pushPtySize();
    };

    layoutPhone();
    requestAnimationFrame(layoutPhone);
    const ro = new ResizeObserver(() => layoutPhone());
    ro.observe(host);
    ro.observe(wrap);

    // ── 同步滚动：SGR 滚轮（对齐桌面 terminal.rs encode_wheel_sgr）──
    // 绝不发 ↑↓：Claude 在输入框聚焦时会当成 prompt 历史，并提示
    // 「Scroll wheel is sending arrow keys · use PgUp/PgDn to scroll」。
    let touchLastY = 0;
    let touching = false;
    let accPx = 0;

    const isAltScreen = () => {
      try {
        return term.buffer.active === term.buffer.alternate;
      } catch {
        return false;
      }
    };

    const canViewportScroll = () => {
      const vp = host.querySelector(".xterm-viewport") as HTMLElement | null;
      return !!vp && vp.scrollHeight > vp.clientHeight + 2;
    };

    /**
     * SGR 鼠标滚轮：`\x1b[<64;col;rowM` 上滚 / `65` 下滚。
     * 与 workspace/terminal.rs encode_wheel_sgr 一致。
     */
    const encodeWheelSgr = (wheelUp: boolean, count: number): string => {
      const cb = wheelUp ? 64 : 65;
      // 点在视口中部，避免点在底部 prompt 条上
      const col = Math.max(1, Math.floor(term.cols / 2));
      const row = Math.max(1, Math.floor(term.rows * 0.35));
      const one = `\x1b[<${cb};${col};${row}M`;
      return one.repeat(Math.min(Math.max(count, 1), 6));
    };

    /** 写入滚动序列（与 Composer 相同 POST /input） */
    const postScrollBytes = (payload: string) => {
      if (!writeEnabled || !payload) return;
      if (transport.mode === "rtc" && transport.rtc) {
        transport.rtc.postInput(sessionId, payload);
      } else {
        void postInput(sessionId, payload);
      }
    };

    const sendAppScroll = (lines: number) => {
      if (!lines || !writeEnabled) return;
      // lines>0：指上滑 / 滚轮下 → 内容上移 → wheel down (65)
      // lines<0：指下滑 → 看更早 → wheel up (64)
      const wheelUp = lines < 0;
      const n = Math.min(Math.abs(lines), 6);
      // 主策略 SGR 滚轮；大步时再夹 PgUp/PgDn（Claude 官方提示）
      let payload = encodeWheelSgr(wheelUp, n);
      if (n >= 3) {
        payload += (wheelUp ? "\x1b[5~" : "\x1b[6~").repeat(Math.min(2, Math.floor(n / 3)));
      }
      postScrollBytes(payload);
    };

    const applyScrollDelta = (dy: number, e?: TouchEvent | WheelEvent) => {
      if (Math.abs(dy) < 0.5) return;
      e?.preventDefault();
      e?.stopPropagation();

      // 主缓冲真 scrollback：本地即可
      if (!isAltScreen() && canViewportScroll()) {
        const vp = host.querySelector(".xterm-viewport") as HTMLElement;
        const prev = vp.scrollTop;
        vp.scrollTop = prev + dy;
        if (vp.scrollTop !== prev) return;
      }

      if (!writeEnabled) return;
      accPx += dy;
      const { h: cellH } = getCellSize();
      const step = Math.max(cellH * 0.55, 12);
      if (Math.abs(accPx) < step) return;
      const lines = Math.trunc(accPx / step);
      accPx -= lines * step;
      sendAppScroll(lines);
    };

    const onTouchStart = (ev: TouchEvent) => {
      if (ev.touches.length !== 1) return;
      touching = true;
      touchLastY = ev.touches[0].clientY;
      accPx = 0;
      // 点在终端上也不让 xterm textarea 聚焦
      unlockFocus = lockTermFocus() ?? unlockFocus;
    };
    const onTouchMove = (ev: TouchEvent) => {
      if (!touching || ev.touches.length !== 1) return;
      const y = ev.touches[0].clientY;
      const dy = touchLastY - y;
      touchLastY = y;
      applyScrollDelta(dy, ev);
    };
    const onTouchEnd = () => {
      touching = false;
      accPx = 0;
    };
    const onWheel = (ev: WheelEvent) => applyScrollDelta(ev.deltaY, ev);
    const onMouseDown = (ev: MouseEvent) => {
      // 阻止默认聚焦行为
      ev.preventDefault();
      unlockFocus = lockTermFocus() ?? unlockFocus;
    };

    const cap = { passive: false, capture: true } as const;
    wrap.addEventListener("touchstart", onTouchStart, { passive: true, capture: true });
    wrap.addEventListener("touchmove", onTouchMove, cap);
    wrap.addEventListener("touchend", onTouchEnd, { passive: true, capture: true });
    wrap.addEventListener("touchcancel", onTouchEnd, { passive: true, capture: true });
    wrap.addEventListener("wheel", onWheel, cap);
    wrap.addEventListener("mousedown", onMouseDown, cap);
    host.addEventListener("wheel", onWheel, cap);
    host.addEventListener("touchmove", onTouchMove, cap);

    const emitBufferText = () => {
      if (!onBufRef.current) return;
      try {
        const { text } = serializeVisibleScreen(term);
        onBufRef.current(text.split("\n").slice(-50).join("\n"));
      } catch {
        /* ignore */
      }
    };

    let bufTimer: number | null = null;
    const scheduleBufEmit = () => {
      if (bufTimer != null) window.clearTimeout(bufTimer);
      bufTimer = window.setTimeout(() => {
        bufTimer = null;
        emitBufferText();
      }, 100);
    };

    let closed = false;
    let retry = 1000;
    let ws: WebSocket | null = null;
    let stopRtc: (() => void) | null = null;

    const measureSoon = () => {
      requestAnimationFrame(() => {
        layoutPhone();
        if (resizeTimer2 != null) window.clearTimeout(resizeTimer2);
        resizeTimer2 = window.setTimeout(() => layoutPhone(), 100);
      });
    };

    const writePtyBytes = (bytes: Uint8Array) => {
      const shouldStickBottom = (() => {
        try {
          const buf = term.buffer.active;
          return buf.viewportY >= buf.baseY + term.rows - 3;
        } catch {
          return true;
        }
      })();
      term.write(bytes, () => {
        if (shouldStickBottom && !isAltScreen()) {
          try {
            term.scrollToBottom();
          } catch {
            /* ignore */
          }
        }
        scheduleBufEmit();
      });
    };

    const connect = () => {
      if (closed) return;

      if (transport.mode === "rtc" && transport.rtc) {
        term.reset();
        lastCols = 0;
        lastRows = 0;
        measureSoon();
        unlockFocus = lockTermFocus() ?? unlockFocus;
        stopRtc = transport.rtc.openPty(sessionId, writePtyBytes);
        return;
      }

      ws = new WebSocket(wsUrl(`/s/${encodeURIComponent(sessionId)}/stream`));
      ws.binaryType = "arraybuffer";
      ws.onopen = () => {
        retry = 1000;
        term.reset();
        lastCols = 0;
        lastRows = 0;
        measureSoon();
        unlockFocus = lockTermFocus() ?? unlockFocus;
      };
      ws.onmessage = (ev) => {
        if (typeof ev.data === "string") return;
        writePtyBytes(new Uint8Array(ev.data as ArrayBuffer));
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
      stopRtc?.();
      stopRtc = null;
      if (resizeTimer != null) window.clearTimeout(resizeTimer);
      if (resizeTimer2 != null) window.clearTimeout(resizeTimer2);
      if (bufTimer != null) window.clearTimeout(bufTimer);
      unlockFocus?.();
      wrap.removeEventListener("touchstart", onTouchStart, true);
      wrap.removeEventListener("touchmove", onTouchMove, true);
      wrap.removeEventListener("touchend", onTouchEnd, true);
      wrap.removeEventListener("touchcancel", onTouchEnd, true);
      wrap.removeEventListener("wheel", onWheel, true);
      wrap.removeEventListener("mousedown", onMouseDown, true);
      host.removeEventListener("wheel", onWheel, true);
      host.removeEventListener("touchmove", onTouchMove, true);
      ro.disconnect();
      ws?.close();
      term.dispose();
    };
  }, [sessionId, writeEnabled, transport.mode, transport.rtc]);

  return (
    <div ref={wrapRef} class={`relative h-full w-full min-h-0 overflow-hidden ${cls ?? ""}`}>
      <div ref={hostRef} class="xterm-host h-full w-full" />
    </div>
  );
}
