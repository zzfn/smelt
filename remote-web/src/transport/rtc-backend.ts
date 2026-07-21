/**
 * 跨网 RTC 后端：DataChannel 帧映射 + 换网自动重连 + 送达确认。
 */

import { connectRtc, parseRtcQuery, type RtcSession } from "./rtc-peer";
import type { RtcConnPhase } from "./types";
import { decodeFrame, encodeFrame, b64ToBytes, type DcFrame } from "./frames";
import type { PermissionMenu, SessionInfo } from "../api";

export type RtcBackend = {
  phase: () => RtcConnPhase;
  waitReady: () => Promise<void>;
  fetchSessions: () => Promise<SessionInfo[]>;
  fetchMenu: (id: string) => Promise<PermissionMenu | null>;
  openPty: (id: string, onBytes: (data: Uint8Array) => void) => () => void;
  /** 成功送达并收到 bridge ack 才 resolve ok */
  postInput: (id: string, data: string) => Promise<{ ok: boolean; err?: string }>;
  postResize: (
    id: string,
    cols: number,
    rows: number,
    cellW?: number,
    cellH?: number,
  ) => void;
  postAction: (
    id: string,
    kind: string,
    text?: string,
  ) => Promise<{ ok: boolean; err?: string }>;
  /** 订阅某会话 phase 更新 */
  subscribeState: (
    id: string,
    onState: (s: { phase?: string; pending_question?: string | null }) => void,
  ) => () => void;
  writeEnabled: () => boolean;
  close: () => void;
};

type Pending = {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
};

type AckPending = {
  resolve: (v: { ok: boolean; err?: string }) => void;
  timer: number;
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
  const pendingMenus = new Map<string, Pending>();
  const pendingAcks: AckPending[] = [];
  const ptyHandlers = new Map<string, (data: Uint8Array) => void>();
  const stateHandlers = new Map<
    string,
    Set<(s: { phase?: string; pending_question?: string | null }) => void>
  >();

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
    for (const [, p] of pendingMenus) {
      p.reject(new Error(reason));
    }
    pendingMenus.clear();
    for (const a of pendingAcks.splice(0)) {
      window.clearTimeout(a.timer);
      a.resolve({ ok: false, err: reason });
    }
  }

  function trySend(frame: DcFrame): boolean {
    if (!session) return false;
    const ph = session.getPhase();
    if (ph !== "connected" && ph !== "ice") {
      // ice 时 DC 可能尚未 open
    }
    return session.sendFrame(frame);
  }

  function sendWithAck(
    frame: DcFrame,
    timeoutMs = 8000,
  ): Promise<{ ok: boolean; err?: string }> {
    return new Promise((resolve) => {
      if (!trySend(frame)) {
        resolve({ ok: false, err: "not connected" });
        return;
      }
      const timer = window.setTimeout(() => {
        const i = pendingAcks.indexOf(entry);
        if (i >= 0) pendingAcks.splice(i, 1);
        resolve({ ok: false, err: "ack timeout" });
      }, timeoutMs);
      const entry: AckPending = { resolve, timer };
      pendingAcks.push(entry);
    });
  }

  function handleFrame(frame: DcFrame) {
    switch (frame.t) {
      case "hello_ok":
        write = frame.write;
        reconnectAttempt = 0;
        reportPhase("connected", "hello_ok");
        for (const id of ptyHandlers.keys()) {
          trySend({ t: "open", id });
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
      case "state": {
        const handlers = stateHandlers.get(frame.id);
        if (handlers) {
          const s = {
            phase: frame.phase,
            pending_question: frame.pending_question,
          };
          for (const fn of handlers) fn(s);
        }
        break;
      }
      case "menu_ok": {
        const p = pendingMenus.get(frame.id);
        if (p) {
          pendingMenus.delete(frame.id);
          p.resolve(frame.menu);
        }
        break;
      }
      case "ack": {
        const a = pendingAcks.shift();
        if (a) {
          window.clearTimeout(a.timer);
          a.resolve({ ok: frame.ok, err: frame.err });
        }
        break;
      }
      case "err": {
        const p = pendingSessions.shift();
        p?.reject(new Error(frame.msg));
        // 失败的 menu
        if (pendingMenus.size) {
          const first = pendingMenus.keys().next().value;
          if (first) {
            pendingMenus.get(first)?.reject(new Error(frame.msg));
            pendingMenus.delete(first);
          }
        }
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

  /** 强制排队重连（busy 时仍保证稍后会再试） */
  function forceReconnect(reason: string) {
    if (stopped) return;
    if (reconnectTimer != null) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (connecting) {
      // 当前连接结束后再来一轮
      reconnectTimer = window.setTimeout(() => {
        reconnectTimer = null;
        if (!stopped && lastPhase !== "connected") {
          reconnectAttempt = Math.max(1, reconnectAttempt);
          void connectOnce();
        }
      }, 500);
      reportPhase("reconnecting", reason);
      return;
    }
    reconnectAttempt = Math.max(1, reconnectAttempt);
    reportPhase("reconnecting", reason);
    void connectOnce();
  }

  const onOnline = () => {
    if (stopped) return;
    if (lastPhase === "connected") return;
    forceReconnect("网络已恢复，正在重连…");
  };
  window.addEventListener("online", onOnline);

  const onVis = () => {
    if (stopped || document.visibilityState !== "visible") return;
    if (
      lastPhase === "failed" ||
      lastPhase === "reconnecting" ||
      lastPhase === "closed" ||
      lastPhase === "idle"
    ) {
      forceReconnect("页面回到前台，正在重连…");
    }
  };
  document.addEventListener("visibilitychange", onVis);

  await connectOnce();

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
        if (!trySend({ t: "sessions" })) {
          reject(new Error("not connected"));
          return;
        }
        pendingSessions.push({
          resolve: (v) => resolve(v as SessionInfo[]),
          reject,
        });
        window.setTimeout(() => {
          const idx = pendingSessions.findIndex((p) => p.reject === reject);
          if (idx >= 0) {
            pendingSessions.splice(idx, 1)[0]?.reject(new Error("sessions timeout"));
          }
        }, 12_000);
      }),
    fetchMenu: (id) =>
      new Promise<PermissionMenu | null>((resolve, reject) => {
        if (!trySend({ t: "menu", id })) {
          resolve(null);
          return;
        }
        pendingMenus.set(id, {
          resolve: (v) => {
            if (v == null) resolve(null);
            else resolve(v as PermissionMenu);
          },
          reject,
        });
        window.setTimeout(() => {
          if (pendingMenus.has(id)) {
            pendingMenus.delete(id);
            resolve(null);
          }
        }, 8_000);
      }),
    openPty(id, onBytes) {
      ptyHandlers.set(id, onBytes);
      trySend({ t: "open", id });
      return () => {
        ptyHandlers.delete(id);
        trySend({ t: "close", id });
      };
    },
    postInput: (id, data) => sendWithAck({ t: "input", id, data }),
    postResize(id, cols, rows, cellW, cellH) {
      trySend({
        t: "resize",
        id,
        cols,
        rows,
        cell_w: cellW,
        cell_h: cellH,
      });
    },
    postAction: (id, kind, text) =>
      sendWithAck({ t: "action", id, kind, text }),
    subscribeState(id, onState) {
      let set = stateHandlers.get(id);
      if (!set) {
        set = new Set();
        stateHandlers.set(id, set);
      }
      set.add(onState);
      // 确保已 open（会带 state watch）
      if (!ptyHandlers.has(id)) {
        trySend({ t: "open", id });
      }
      return () => {
        set?.delete(onState);
        if (set && set.size === 0) stateHandlers.delete(id);
      };
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
