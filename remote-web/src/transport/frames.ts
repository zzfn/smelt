/**
 * WebRTC DataChannel 上的业务帧（webrtc-edge 方案 α）。
 * Bridge 与手机 SPA 共用此形状；JSON 文本帧，binary 后续可加。
 */

export type DcFrame =
  | { t: "hello"; token: string; write?: boolean }
  | { t: "hello_ok"; write: boolean }
  | { t: "sessions" }
  | {
      t: "sessions_ok";
      sessions: Array<{
        id: string;
        name: string;
        project: string;
        phase: string;
        parent_session?: string | null;
        cwd?: string | null;
      }>;
    }
  | { t: "open"; id: string }
  | { t: "open_ok"; id: string }
  | { t: "close"; id: string }
  /** 下行 PTY：base64 原始字节 */
  | { t: "pty"; id: string; data: string }
  | { t: "input"; id: string; data: string }
  | { t: "action"; id: string; kind: string; text?: string }
  | {
      t: "resize";
      id: string;
      cols: number;
      rows: number;
      cell_w?: number;
      cell_h?: number;
    }
  | { t: "state"; id: string; phase?: string; pending_question?: string | null }
  | { t: "err"; msg: string; code?: string };

export function encodeFrame(frame: DcFrame): string {
  return JSON.stringify(frame);
}

export function decodeFrame(raw: string): DcFrame | null {
  try {
    const v = JSON.parse(raw) as DcFrame;
    if (!v || typeof v !== "object" || typeof (v as { t?: unknown }).t !== "string") {
      return null;
    }
    return v;
  } catch {
    return null;
  }
}

/** Uint8Array → base64（pty 下行） */
export function bytesToB64(bytes: Uint8Array): string {
  let s = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    s += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(s);
}

export function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
