import { useEffect, useRef } from "preact/hooks";
import { Terminal } from "@xterm/xterm";
import { wsUrl } from "../api";
import { serializeVisibleScreen } from "../lib/serializeScreen";

type Props = {
  sessionId: string;
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

const FONT_FAMILY =
  'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Monaco, Consolas, monospace';

/**
 * 移动端终端面 —— **镜像模式**（内容与 PC 一致，交互本地独立）
 *
 * | 维度 | 策略 |
 * |------|------|
 * | 内容 | 同一 watch 字节流 + 同一 PTY 行列（stream header） |
 * | 尺寸 | **不** postResize；只改本机 fontSize / CSS scale 适配手机视口 |
 * | 滚动 | 纯本地 viewport / 带色历史层；不向 PTY 发方向键 |
 * | 输入 | 仍走 POST /input（可选写入），与 PC 共享会话状态 |
 */
export function XtermSurface({
  sessionId,
  onUserData,
  writeEnabled,
  class: cls,
  onBufferText,
}: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const histHostRef = useRef<HTMLDivElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const onDataRef = useRef(onUserData);
  onDataRef.current = onUserData;
  const onBufRef = useRef(onBufferText);
  onBufRef.current = onBufferText;

  useEffect(() => {
    const host = hostRef.current;
    const histHost = histHostRef.current;
    const wrap = wrapRef.current;
    if (!host || !histHost || !wrap) return;

    const termOpts = {
      convertEol: false as const,
      fontSize: 12,
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
    };

    const term = new Terminal({
      ...termOpts,
      cursorBlink: !!writeEnabled,
      disableStdin: !writeEnabled,
    });
    const histTerm = new Terminal({
      ...termOpts,
      cursorBlink: false,
      disableStdin: true,
      scrollback: 12000,
    });

    term.open(host);
    histTerm.open(histHost);

    const core = term as unknown as { _core?: CoreDims };
    const getCellSize = () => {
      const cell = core._core?._renderService?.dimensions?.css?.cell;
      if (cell && cell.width > 0 && cell.height > 0) {
        return { w: cell.width, h: cell.height };
      }
      const fs = Number(term.options.fontSize) || 12;
      return { w: fs * 0.62, h: fs * 1.2 };
    };

    // PTY 几何：以 stream header / PC 为准，手机绝不改 PTY
    let ptyCols = 80;
    let ptyRows = 24;
    let geometryReady = false;

    const applyScale = (el: HTMLElement, t: Terminal) => {
      const xtermEl = el.querySelector(".xterm") as HTMLElement | null;
      if (!xtermEl) return;
      const { w: cellW, h: cellH } = (() => {
        if (t === term) return getCellSize();
        const c = (histTerm as unknown as { _core?: CoreDims })._core?._renderService
          ?.dimensions?.css?.cell;
        if (c && c.width > 0 && c.height > 0) return { w: c.width, h: c.height };
        return getCellSize();
      })();
      const contentW = Math.max(t.cols * cellW, 1);
      const contentH = Math.max(t.rows * cellH, 1);
      const availW = Math.max(el.clientWidth, 1);
      const availH = Math.max(el.clientHeight, 1);
      // 完整装进手机视口（可缩小；略放大上限 1.25 避免大屏空旷）
      const s = Math.min(availW / contentW, availH / contentH, 1.25);
      const scaledW = contentW * s;
      const scaledH = contentH * s;
      const ox = (availW - scaledW) / 2;
      const oy = (availH - scaledH) / 2;
      xtermEl.style.width = `${contentW}px`;
      xtermEl.style.height = `${contentH}px`;
      xtermEl.style.transformOrigin = "top left";
      xtermEl.style.transform = `translate(${Math.max(0, ox)}px, ${Math.max(0, oy)}px) scale(${s})`;
    };

    /**
     * 按 PTY 行列布置本机 xterm，再用字号 + scale 适配宿主。
     * 不调用 /resize，PC 几何不受影响。
     */
    const layoutMirror = () => {
      if (host.clientWidth < 20 || host.clientHeight < 20) return;
      const availW = Math.max(host.clientWidth - 2, 20);
      const availH = Math.max(host.clientHeight - 2, 20);
      // 先按近似 cell 比例估字号，使 cols×rows 大致铺满
      let fs = Math.min(availW / ptyCols / 0.6, availH / ptyRows / 1.2);
      fs = Math.max(5, Math.min(20, Math.floor(fs * 10) / 10));

      term.options.fontSize = fs;
      histTerm.options.fontSize = fs;
      try {
        term.resize(ptyCols, ptyRows);
        histTerm.resize(ptyCols, ptyRows);
      } catch {
        /* ignore */
      }

      // 实测 cell 后微调字号，避免最后一列被裁
      requestAnimationFrame(() => {
        const { w: cellW, h: cellH } = getCellSize();
        const needW = ptyCols * cellW;
        const needH = ptyRows * cellH;
        if (needW > availW + 0.5 || needH > availH + 0.5) {
          const factor = Math.min(availW / needW, availH / needH);
          const fs2 = Math.max(5, Math.floor(fs * factor * 10) / 10);
          if (Math.abs(fs2 - fs) > 0.05) {
            term.options.fontSize = fs2;
            histTerm.options.fontSize = fs2;
            try {
              term.resize(ptyCols, ptyRows);
              histTerm.resize(ptyCols, ptyRows);
            } catch {
              /* ignore */
            }
          }
        }
        applyScale(host, term);
        if (histHost.style.display !== "none") {
          applyScale(histHost, histTerm);
        }
      });
    };

    const setPtyGeometry = (cols: number, rows: number) => {
      const c = Math.max(2, Math.min(300, Math.floor(cols)));
      const r = Math.max(2, Math.min(200, Math.floor(rows)));
      if (geometryReady && c === ptyCols && r === ptyRows) {
        layoutMirror();
        return;
      }
      ptyCols = c;
      ptyRows = r;
      geometryReady = true;
      layoutMirror();
    };

    // 默认在收到 header 前先按 80×24 占位
    setPtyGeometry(80, 24);

    const ro = new ResizeObserver(() => layoutMirror());
    ro.observe(host);
    ro.observe(wrap);

    let dataDisp: { dispose: () => void } | undefined;
    if (writeEnabled) {
      dataDisp = term.onData((d) => onDataRef.current?.(d));
    }

    // ── 本地独立滚动 + 带色历史 ─────────────────────────────
    let followLive = true;
    let lastCapText = "";
    let lastCapAnsi = "";
    let histHasContent = false;
    let touchLastY = 0;
    let touching = false;
    let accPx = 0;

    const liveBtn = document.createElement("button");
    liveBtn.type = "button";
    liveBtn.className = "xterm-live-btn";
    liveBtn.textContent = "↓ 回到实时";
    liveBtn.style.display = "none";
    liveBtn.addEventListener("click", () => goLive());
    wrap.appendChild(liveBtn);

    const isAltScreen = () => {
      try {
        return term.buffer.active === term.buffer.alternate;
      } catch {
        return false;
      }
    };

    const canViewportScroll = (el: HTMLElement) => {
      const vp = el.querySelector(".xterm-viewport") as HTMLElement | null;
      return !!vp && vp.scrollHeight > vp.clientHeight + 2;
    };

    const appendFrameToHist = (ansi: string) => {
      if (!ansi.trim()) return;
      try {
        histTerm.write(ansi.endsWith("\n") ? ansi + "\r\n" : ansi + "\r\n\r\n");
        histHasContent = true;
      } catch {
        /* ignore */
      }
    };

    const accumulateHistory = () => {
      if (!followLive) return;
      const { ansi, text } = serializeVisibleScreen(term);
      if (text === lastCapText) return;
      if (lastCapAnsi) appendFrameToHist(lastCapAnsi);
      lastCapAnsi = ansi;
      lastCapText = text;
    };

    const enterBrowse = () => {
      if (!followLive) return;
      accumulateHistory();
      if (lastCapAnsi) appendFrameToHist(lastCapAnsi);
      followLive = false;
      histHost.style.display = "block";
      host.style.visibility = "hidden";
      liveBtn.style.display = "block";
      layoutMirror();
      try {
        histTerm.scrollToBottom();
        histTerm.scrollLines(-Math.max(2, Math.floor(histTerm.rows * 0.15)));
      } catch {
        /* ignore */
      }
    };

    const goLive = () => {
      followLive = true;
      histHost.style.display = "none";
      host.style.visibility = "visible";
      liveBtn.style.display = "none";
      accPx = 0;
      layoutMirror();
      try {
        term.scrollToBottom();
      } catch {
        /* ignore */
      }
    };

    const scrollTerm = (el: HTMLElement, t: Terminal, dy: number): boolean => {
      if (canViewportScroll(el)) {
        const vp = el.querySelector(".xterm-viewport") as HTMLElement;
        const prev = vp.scrollTop;
        vp.scrollTop = prev + dy;
        if (vp.scrollTop !== prev) return true;
      }
      const { h: cellH } = getCellSize();
      const step = Math.max(cellH * 0.45, 8);
      accPx += dy;
      if (Math.abs(accPx) < step) return Math.abs(dy) >= 0.5;
      const lines = Math.trunc(accPx / step);
      accPx -= lines * step;
      if (lines !== 0) {
        t.scrollLines(lines);
        return true;
      }
      return false;
    };

    const histAtBottom = () => {
      try {
        const buf = histTerm.buffer.active;
        return buf.viewportY >= buf.baseY + histTerm.rows - 2;
      } catch {
        return true;
      }
    };

    const applyScrollDelta = (dy: number, e?: TouchEvent | WheelEvent) => {
      if (Math.abs(dy) < 0.5) return;

      if (!followLive) {
        scrollTerm(histHost, histTerm, dy);
        e?.preventDefault();
        if (dy > 0 && histAtBottom()) goLive();
        return;
      }

      if (canViewportScroll(host)) {
        const vp = host.querySelector(".xterm-viewport") as HTMLElement;
        const prev = vp.scrollTop;
        vp.scrollTop = prev + dy;
        if (vp.scrollTop !== prev) {
          e?.preventDefault();
          return;
        }
        if (dy < 0 && vp.scrollTop <= 0 && (histHasContent || lastCapAnsi)) {
          enterBrowse();
          e?.preventDefault();
          return;
        }
      }

      accPx += dy;
      const { h: cellH } = getCellSize();
      const step = Math.max(cellH * 0.45, 8);
      if (Math.abs(accPx) < step) {
        e?.preventDefault();
        return;
      }
      const lines = Math.trunc(accPx / step);
      accPx -= lines * step;

      if (lines < 0) {
        accumulateHistory();
        if (histHasContent || lastCapAnsi) {
          enterBrowse();
        } else if (!isAltScreen()) {
          term.scrollLines(lines);
        }
        e?.preventDefault();
        return;
      }

      if (!isAltScreen()) term.scrollLines(lines);
      e?.preventDefault();
    };

    const onTouchStart = (ev: TouchEvent) => {
      if (ev.touches.length !== 1) return;
      touching = true;
      touchLastY = ev.touches[0].clientY;
      accPx = 0;
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

    wrap.addEventListener("touchstart", onTouchStart, { passive: true });
    wrap.addEventListener("touchmove", onTouchMove, { passive: false });
    wrap.addEventListener("touchend", onTouchEnd, { passive: true });
    wrap.addEventListener("touchcancel", onTouchEnd, { passive: true });
    wrap.addEventListener("wheel", onWheel, { passive: false });

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

    const connect = () => {
      if (closed) return;
      ws = new WebSocket(wsUrl(`/s/${encodeURIComponent(sessionId)}/stream`));
      ws.binaryType = "arraybuffer";
      ws.onopen = () => {
        retry = 1000;
        term.reset();
        histTerm.reset();
        lastCapText = "";
        lastCapAnsi = "";
        histHasContent = false;
        followLive = true;
        histHost.style.display = "none";
        host.style.visibility = "visible";
        liveBtn.style.display = "none";
      };
      ws.onmessage = (ev) => {
        // 文本帧 = PTY 尺寸 header（PC / smeltd 为准）
        if (typeof ev.data === "string") {
          try {
            const j = JSON.parse(ev.data) as { cols?: number; rows?: number };
            if (j.cols && j.rows) {
              setPtyGeometry(j.cols, j.rows);
            }
          } catch {
            /* ignore */
          }
          return;
        }
        const bytes = new Uint8Array(ev.data as ArrayBuffer);
        const shouldStickBottom =
          followLive &&
          (() => {
            try {
              const buf = term.buffer.active;
              return buf.viewportY >= buf.baseY + term.rows - 3;
            } catch {
              return true;
            }
          })();

        term.write(bytes, () => {
          if (followLive) {
            accumulateHistory();
            if (shouldStickBottom && !isAltScreen()) {
              try {
                term.scrollToBottom();
              } catch {
                /* ignore */
              }
            }
          }
          scheduleBufEmit();
        });
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
      if (bufTimer != null) window.clearTimeout(bufTimer);
      wrap.removeEventListener("touchstart", onTouchStart);
      wrap.removeEventListener("touchmove", onTouchMove);
      wrap.removeEventListener("touchend", onTouchEnd);
      wrap.removeEventListener("touchcancel", onTouchEnd);
      wrap.removeEventListener("wheel", onWheel);
      liveBtn.remove();
      dataDisp?.dispose();
      ro.disconnect();
      ws?.close();
      histTerm.dispose();
      term.dispose();
    };
  }, [sessionId, writeEnabled]);

  return (
    <div ref={wrapRef} class={`relative h-full w-full min-h-0 overflow-hidden ${cls ?? ""}`}>
      <div ref={hostRef} class="xterm-host h-full w-full" />
      <div
        ref={histHostRef}
        class="xterm-host xterm-hist-host h-full w-full"
        style={{ display: "none" }}
      />
    </div>
  );
}
