/**
 * 浏览器原生 WebRTC（RTCPeerConnection + RTCDataChannel）。
 * 无 simple-peer / PeerJS；信令经外部 WebSocket 注入。
 *
 * 角色：手机端为 **client**（收 offer 或发 offer 由协议约定；此处 client 在
 * peer_joined 后创建 offer，host/bridge 回 answer）。
 */

import { connectSignaling, iceServersFromHello } from "./signaling";
import type { RtcConnectOptions, RtcConnPhase, SignalingMessage, SignalPayload } from "./types";
import { encodeFrame, type DcFrame } from "./frames";

const DC_LABEL = "smelt";

export type RtcSession = {
  /** DataChannel 就绪后可发业务帧 */
  sendFrame: (frame: DcFrame) => void;
  /** 原始字符串（已 JSON） */
  sendRaw: (raw: string) => void;
  close: () => void;
  getPhase: () => RtcConnPhase;
};

export async function connectRtc(opts: RtcConnectOptions): Promise<RtcSession> {
  let phase: RtcConnPhase = "idle";
  const setPhase = (p: RtcConnPhase, detail?: string) => {
    phase = p;
    opts.onPhase?.(p, detail);
  };

  setPhase("signaling");

  let pc: RTCPeerConnection | null = null;
  let dc: RTCDataChannel | null = null;
  let iceServers: RTCIceServer[] = [{ urls: "stun:stun.l.google.com:19302" }];

  const pendingIce: RTCIceCandidateInit[] = [];
  let remoteDescSet = false;

  const signal = connectSignaling(opts.signalUrl, {
    onOpen() {
      signal.send({
        op: "hello",
        role: "client",
        room: opts.room,
        secret: opts.secret,
      });
    },
    onMessage(msg) {
      void handleSignal(msg);
    },
    onClose() {
      if (phase !== "closed" && phase !== "failed") {
        setPhase("failed", "signaling closed");
        opts.onClose?.("signaling closed");
      }
    },
    onError() {
      setPhase("failed", "signaling error");
    },
  });

  async function handleSignal(msg: SignalingMessage) {
    switch (msg.op) {
      case "hello_ok": {
        iceServers = iceServersFromHello(msg);
        ensurePc();
        break;
      }
      case "peer_joined": {
        if (msg.role === "host") {
          // Bridge 上线或重连：整页新 PC + 新 offer（避免复用旧 ICE）
          resetPc();
          ensurePc();
          await createAndSendOffer();
        }
        break;
      }
      case "peer_left": {
        if (msg.role === "host") {
          setPhase("failed", "host left");
          opts.onClose?.("host left");
        }
        break;
      }
      case "signal": {
        if (msg.from !== "host") return;
        await applyRemoteSignal(msg.payload);
        break;
      }
      case "err": {
        setPhase("failed", msg.msg);
        opts.onClose?.(msg.msg);
        break;
      }
      case "ping":
        signal.send({ op: "pong" });
        break;
      default:
        break;
    }
  }

  function resetPc() {
    try {
      dc?.close();
    } catch {
      /* ignore */
    }
    try {
      pc?.close();
    } catch {
      /* ignore */
    }
    dc = null;
    pc = null;
    remoteDescSet = false;
    pendingIce.length = 0;
  }

  function ensurePc() {
    if (pc) return;
    setPhase("ice");
    pc = new RTCPeerConnection({ iceServers });

    pc.onicecandidate = (ev) => {
      const payload: SignalPayload = ev.candidate
        ? { kind: "ice", candidate: ev.candidate.toJSON() }
        : { kind: "ice", candidate: null };
      signal.send({ op: "signal", from: "client", payload });
    };

    pc.onconnectionstatechange = () => {
      const s = pc?.connectionState;
      if (s === "connected") setPhase("connected");
      else if (s === "failed") {
        setPhase("failed", "pc failed");
        opts.onClose?.("peer connection failed");
      } else if (s === "closed" || s === "disconnected") {
        if (phase === "connected") {
          setPhase("closed");
          opts.onClose?.("disconnected");
        }
      }
    };

    pc.ondatachannel = (ev) => {
      // 若 host 创建 channel
      wireDc(ev.channel);
    };

    // client 主动建 channel（与 bridge 约定：client create）
    const channel = pc.createDataChannel(DC_LABEL, {
      ordered: true,
    });
    wireDc(channel);
  }

  function wireDc(channel: RTCDataChannel) {
    dc = channel;
    channel.binaryType = "arraybuffer";
    channel.onopen = () => {
      setPhase("connected");
      // 业务握手
      sendRaw(
        encodeFrame({
          t: "hello",
          token: opts.token,
          write: opts.write,
        }),
      );
    };
    channel.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        opts.onFrame?.(ev.data);
      } else if (ev.data instanceof ArrayBuffer) {
        // 预留：二进制 pty 帧
        const text = new TextDecoder().decode(ev.data);
        opts.onFrame?.(text);
      }
    };
    channel.onclose = () => {
      if (phase !== "closed") {
        setPhase("closed");
        opts.onClose?.("datachannel closed");
      }
    };
  }

  async function createAndSendOffer() {
    if (!pc) return;
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    signal.send({
      op: "signal",
      from: "client",
      payload: { kind: "offer", sdp: offer.sdp || "" },
    });
  }

  async function applyRemoteSignal(payload: SignalPayload) {
    ensurePc();
    if (!pc) return;

    if (payload.kind === "offer") {
      await pc.setRemoteDescription({ type: "offer", sdp: payload.sdp });
      remoteDescSet = true;
      await flushIce();
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      signal.send({
        op: "signal",
        from: "client",
        payload: { kind: "answer", sdp: answer.sdp || "" },
      });
    } else if (payload.kind === "answer") {
      await pc.setRemoteDescription({ type: "answer", sdp: payload.sdp });
      remoteDescSet = true;
      await flushIce();
    } else if (payload.kind === "ice") {
      if (payload.candidate) {
        if (remoteDescSet) {
          try {
            await pc.addIceCandidate(payload.candidate);
          } catch {
            /* ignore race */
          }
        } else {
          pendingIce.push(payload.candidate);
        }
      }
    }
  }

  async function flushIce() {
    if (!pc) return;
    while (pendingIce.length) {
      const c = pendingIce.shift()!;
      try {
        await pc.addIceCandidate(c);
      } catch {
        /* ignore */
      }
    }
  }

  function sendRaw(raw: string) {
    if (dc && dc.readyState === "open") {
      dc.send(raw);
    }
  }

  function sendFrame(frame: DcFrame) {
    sendRaw(encodeFrame(frame));
  }

  function close() {
    setPhase("closed");
    resetPc();
    signal.close();
  }

  return {
    sendFrame,
    sendRaw,
    close,
    getPhase: () => phase,
  };
}

/**
 * 从 URL query 解析跨网参数：
 *   ?room=AB12&k=secret&signal=wss%3A%2F%2F...
 * token 仍可用 smelt 的 token= 或 room secret 衍生（bridge 侧校验）。
 */
export function parseRtcQuery(
  search: string = location.search,
): { room: string; secret: string; signalUrl: string } | null {
  const q = new URLSearchParams(search);
  const room = q.get("room") || "";
  const secret = q.get("k") || q.get("secret") || "";
  const signalUrl = q.get("signal") || "";
  if (!room || !secret || !signalUrl) return null;
  return { room, secret, signalUrl };
}
