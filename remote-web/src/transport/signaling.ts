/**
 * 原生 WebSocket 信令客户端：只交换 SDP/ICE，不传 PTY。
 * 协议见 docs/webrtc-edge.md。
 */

import type { SignalingMessage, IceServerConfig } from "./types";

export type SignalingClient = {
  send: (msg: SignalingMessage) => void;
  close: () => void;
};

export type SignalingHandlers = {
  onMessage: (msg: SignalingMessage) => void;
  onOpen?: () => void;
  onClose?: (ev: CloseEvent) => void;
  onError?: (ev: Event) => void;
};

export function connectSignaling(url: string, handlers: SignalingHandlers): SignalingClient {
  const ws = new WebSocket(url);

  ws.onopen = () => handlers.onOpen?.();
  ws.onerror = (ev) => handlers.onError?.(ev);
  ws.onclose = (ev) => handlers.onClose?.(ev);
  ws.onmessage = (ev) => {
    if (typeof ev.data !== "string") return;
    try {
      const msg = JSON.parse(ev.data) as SignalingMessage;
      if (msg && typeof msg === "object" && "op" in msg) {
        handlers.onMessage(msg);
      }
    } catch {
      /* ignore malformed */
    }
  };

  return {
    send(msg) {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify(msg));
      }
    },
    close() {
      try {
        ws.close();
      } catch {
        /* ignore */
      }
    },
  };
}

/** 从 hello_ok 取出 ICE 配置；缺省时仅 STUN 便于本地 dev */
export function iceServersFromHello(msg: { ice_servers?: IceServerConfig[] }): RTCIceServer[] {
  const list = msg.ice_servers?.length
    ? msg.ice_servers
    : [{ urls: "stun:stun.l.google.com:19302" }];
  return list.map((s) => ({
    urls: s.urls,
    username: s.username,
    credential: s.credential,
  }));
}
