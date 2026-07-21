/**
 * 跨网 RTC 后端：把 DataChannel 帧映射成接近 api.ts 的调用。
 */

import { connectRtc, parseRtcQuery, type RtcSession } from "./rtc-peer";
import type { RtcConnPhase } from "./types";
import { decodeFrame, encodeFrame, b64ToBytes, type DcFrame } from "./frames";
import type { SessionInfo } from "../api";

export type RtcBackend = {
  phase: () => RtcConnPhase;
  waitReady: () => Promise<void>;
  fetchSessions: () => Promise<SessionInfo[]>;
  openPty: (
    id: string,
    onBytes: (data: Uint8Array) => void,
  ) => () => void;
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
  let readyResolve: () => void = () => {};
  const ready = new Promise<void>((r) => {
    readyResolve = r;
  });
  let readyDone = false;

  const pendingSessions: Pending[] = [];
  const ptyHandlers = new Map<string, (data: Uint8Array) => void>();

  let session: RtcSession | null = null;

  session = await connectRtc({
    signalUrl: q.signalUrl,
    room: q.room,
    secret: q.secret,
    token,
    write: true,
    onPhase,
    onFrame(raw) {
      const frame = decodeFrame(raw);
      if (!frame) return;
      handle(frame);
    },
    onClose(reason) {
      onPhase?.("failed", reason);
      for (const p of pendingSessions.splice(0)) {
        p.reject(new Error(reason));
      }
    },
  });

  function handle(frame: DcFrame) {
    switch (frame.t) {
      case "hello_ok":
        write = frame.write;
        if (!readyDone) {
          readyDone = true;
          readyResolve();
        }
        break;
      case "sessions_ok":
        {
          const list = frame.sessions as SessionInfo[];
          const p = pendingSessions.shift();
          p?.resolve(list);
        }
        break;
      case "pty":
        {
          const h = ptyHandlers.get(frame.id);
          if (h) {
            try {
              h(b64ToBytes(frame.data));
            } catch {
              /* ignore */
            }
          }
        }
        break;
      case "err":
        {
          const p = pendingSessions.shift();
          p?.reject(new Error(frame.msg));
          onPhase?.("failed", frame.msg);
        }
        break;
      case "open_ok":
        break;
      default:
        break;
    }
  }

  // hello 已在 dc onopen 里发；等 hello_ok
  const timeout = window.setTimeout(() => {
    if (!readyDone) {
      readyDone = true;
      readyResolve();
    }
  }, 15000);

  await ready;
  window.clearTimeout(timeout);

  return {
    phase: () => session?.getPhase() ?? "closed",
    waitReady: async () => {},
    writeEnabled: () => write,
    fetchSessions: () =>
      new Promise<SessionInfo[]>((resolve, reject) => {
        pendingSessions.push({
          resolve: (v) => resolve(v as SessionInfo[]),
          reject,
        });
        session?.sendFrame({ t: "sessions" });
        window.setTimeout(() => {
          if (pendingSessions[0]) {
            const p = pendingSessions.shift();
            p?.reject(new Error("sessions timeout"));
          }
        }, 12000);
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
      session?.close();
    },
  };
}

export { parseRtcQuery, encodeFrame };
