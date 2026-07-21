/**
 * 跨网 RTC 后端：DataChannel 帧映射 + 换网自动重连。
 */

import { connectRtc, parseRtcQuery, type RtcSession } from "./rtc-peer";
import type { RtcConnPhase } from "./types";
import { decodeFrame, encodeFrame, b64ToBytes, type DcFrame } from "./frames";
import type { SessionInfo } from "../api";

export type RtcBackend = {
  phase: () => RtcConnPhase;
  waitReady: () => Promise<void>;
  fetchSessions: () => Promise<SessionInfo[]>;
  openPty: (id: string, onBytes: (data: Uint8Array) => void) => () => void;
  postInput: (id: string, data: string) => void;
  postResize: (
    id: string,
    cols: number,
    rows: number,
    cellW?: number,
    cellH?: number,
  ) => void;
  postAction: (id: string, kind: string, text?: string) => void;
  writeEnabled: () => boolean;
  close: () => void;
};

type Pending = {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
};

const MAX_BACKOFF_MS = 12_000;
const BASE_BACKOFF_MS = 800;

export async function startRtcBackend(
  onPhase?: (p: RtcConnPhase, detail?: string) => void,
): Promise<RtcBackend | null> {
  const q = parseRtcQuery();
  if (!q) return null;

  const token =
    new URLSearchParams(location.search).get("token") ||
    sessionStorage.getItem("smelt_token") ||
    q.secret;

  let write = true;
  let session: RtcSession | null = null;
  let stopped = false;
  let reconnectAttempt = 0;
  let reconnectTimer: number | null = null;
  let connecting = false;
  let lastPhase: RtcConnPhase = "idle";

  const pendingSessions: Pending[] = [];
  /** 仍打开的 PTY 订阅：重连后自动 open 恢复 */
  const ptyHandlers = new Map<string, (data: Uint8Array) => void>();

  let firstReadyResolve: (() => void) | null = null;
  let firstReadyDone = false;
  const firstReady = new Promise<void>((r) => {
    firstReadyResolve = r;
  });

  const reportPhase = (p: RtcConnPhase, detail?: string) => {
    lastPhase = p;
    onPhase?.(p, detail);
  };

  function rejectPending(reason: string) {
    for (const p of pendingSessions.splice(0)) {
      p.reject(new Error(reason));
    }
  }

  function handleFrame(frame: DcFrame) {
    switch (frame.t) {
      case "hello_ok":
        write = frame.write;
        reconnectAttempt = 0;
        reportPhase("connected", "hello_ok");
        // 恢复已打开的终端流
        for (const id of ptyHandlers.keys()) {
          session?.sendFrame({ t: "open", id });
        }
        if (!firstReadyDone) {
          firstReadyDone = true;
          firstReadyResolve?.();
        }
        break;
      case "sessions_ok": {
        const list = frame.sessions as SessionInfo[];
        const p = pendingSessions.shift();
        p?.resolve(list);
        break;
      }
      case "pty": {
        const h = ptyHandlers.get(frame.id);
        if (h) {
          try {
            h(b64ToBytes(frame.data));
          } catch {
            /* ignore */
          }
        }
        break;
      }
      case "err": {
        const p = pendingSessions.shift();
        p?.reject(new Error(frame.msg));
        // auth 错误不重连
        if (frame.code === "auth") {
          reportPhase("failed", frame.msg);
          stopped = true;
        }
        break;
      }
      case "open_ok":
        break;
      default:
        break;
    }
  }

  async function connectOnce(): Promise<void> {
    if (stopped || connecting) return;
    connecting = true;
    try {
      // 关掉旧会话（不触发无限 onClose 环：close 为 intentional）
      if (session) {
        try {
          session.close();
        } catch {
          /* ignore */
        }
        session = null;
      }

      reportPhase(
        reconnectAttempt > 0 ? "reconnecting" : "signaling",
        reconnectAttempt > 0 ? `第 ${reconnectAttempt} 次重连…` : undefined,
      );

      session = await connectRtc({
        signalUrl: q.signalUrl,
        room: q.room,
        secret: q.secret,
        token,
        write: true,
        onPhase(p, detail) {
          if (stopped) return;
          // 重连过程中的 failed 由 onClose 统一调度，避免双报
          if (p === "failed") return;
          reportPhase(p, detail);
        },
        onFrame(raw) {
          const frame = decodeFrame(raw);
          if (frame) handleFrame(frame);
        },
        onClose(reason) {
          if (stopped) return;
          rejectPending(reason);
          scheduleReconnect(reason);
        },
      });
    } catch (e) {
      if (!stopped) {
        const msg = e instanceof Error ? e.message : String(e);
        scheduleReconnect(msg);
      }
    } finally {
      connecting = false;
    }
  }

  function scheduleReconnect(reason: string) {
    if (stopped) return;
    if (reconnectTimer != null) return;

    reconnectAttempt += 1;
    const delay = Math.min(
      MAX_BACKOFF_MS,
      BASE_BACKOFF_MS * Math.pow(1.7, Math.min(reconnectAttempt - 1, 8)),
    );
    reportPhase("reconnecting", `${reason} · ${Math.round(delay)}ms 后重试`);

    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      void connectOnce();
    }, delay);
  }

  // 浏览器 online：立刻重连（换网回来）
  const onOnline = () => {
    if (stopped) return;
    if (lastPhase === "connected") return;
    if (reconnectTimer != null) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    reconnectAttempt = Math.max(1, reconnectAttempt);
    reportPhase("reconnecting", "网络已恢复，正在重连…");
    void connectOnce();
  };
  window.addEventListener("online", onOnline);

  // 页面从后台回前台且已断：补一刀
  const onVis = () => {
    if (stopped || document.visibilityState !== "visible") return;
    if (lastPhase === "failed" || lastPhase === "reconnecting" || lastPhase === "closed") {
      if (reconnectTimer != null) {
        window.clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      void connectOnce();
    }
  };
  document.addEventListener("visibilitychange", onVis);

  await connectOnce();

  // 首次等 hello_ok（最多 20s）
  await Promise.race([
    firstReady,
    new Promise<void>((r) => window.setTimeout(r, 20_000)),
  ]);

  return {
    phase: () => session?.getPhase() ?? lastPhase,
    waitReady: async () => {},
    writeEnabled: () => write,
    fetchSessions: () =>
      new Promise<SessionInfo[]>((resolve, reject) => {
        if (!session || session.getPhase() === "failed") {
          reject(new Error("not connected"));
          return;
        }
        pendingSessions.push({
          resolve: (v) => resolve(v as SessionInfo[]),
          reject,
        });
        session.sendFrame({ t: "sessions" });
        window.setTimeout(() => {
          const idx = pendingSessions.findIndex((p) => p.reject === reject);
          if (idx >= 0) {
            pendingSessions.splice(idx, 1)[0]?.reject(new Error("sessions timeout"));
          }
        }, 12_000);
      }),
    openPty(id, onBytes) {
      ptyHandlers.set(id, onBytes);
      session?.sendFrame({ t: "open", id });
      return () => {
        ptyHandlers.delete(id);
        session?.sendFrame({ t: "close", id });
      };
    },
    postInput(id, data) {
      session?.sendFrame({ t: "input", id, data });
    },
    postResize(id, cols, rows, cellW, cellH) {
      session?.sendFrame({
        t: "resize",
        id,
        cols,
        rows,
        cell_w: cellW,
        cell_h: cellH,
      });
    },
    postAction(id, kind, text) {
      session?.sendFrame({ t: "action", id, kind, text });
    },
    close() {
      stopped = true;
      if (reconnectTimer != null) {
        window.clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      window.removeEventListener("online", onOnline);
      document.removeEventListener("visibilitychange", onVis);
      rejectPending("closed");
      try {
        session?.close();
      } catch {
        /* ignore */
      }
      session = null;
    },
  };
}

export { parseRtcQuery, encodeFrame };
