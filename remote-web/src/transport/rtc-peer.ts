/**
 * 浏览器原生 WebRTC（RTCPeerConnection + RTCDataChannel）。
 * 角色：手机端为 **client**（peer_joined host 后发 offer）。
 *
 * 主动 close() 不会触发 onClose；意外断线会触发 onClose，供上层自动重连。
 */

import { connectSignaling, DEFAULT_ICE_SERVERS, iceServersFromHello } from "./signaling";
import type { RtcConnectOptions, RtcConnPhase, SignalingMessage, SignalPayload } from "./types";
import { encodeFrame, type DcFrame } from "./frames";

const DC_LABEL = "smelt";

export type RtcSession = {
  /** 成功入队发送返回 true；DC 未 open 返回 false */
  sendFrame: (frame: DcFrame) => boolean;
  sendRaw: (raw: string) => boolean;
  close: () => void;
  getPhase: () => RtcConnPhase;
};

export async function connectRtc(opts: RtcConnectOptions): Promise<RtcSession> {
  let phase: RtcConnPhase = "idle";
  let intentionalClose = false;

  const setPhase = (p: RtcConnPhase, detail?: string) => {
    phase = p;
    opts.onPhase?.(p, detail);
  };

  const fail = (reason: string) => {
    if (intentionalClose) return;
    if (phase === "failed" || phase === "closed") return;
    setPhase("failed", reason);
    stopRefreshTimer();
    opts.onClose?.(reason);
  };

  setPhase("signaling");

  let pc: RTCPeerConnection | null = null;
  let dc: RTCDataChannel | null = null;
  let iceServers: RTCIceServer[] = [...DEFAULT_ICE_SERVERS];

  const pendingIce: RTCIceCandidateInit[] = [];
  let remoteDescSet = false;

  // TURN REST 临时凭证有过期时间（常见 1h），长连接不刷新中途会悄悄过期，
  // 下次 ICE restart/重建时被拒——是"偶发需要手动开关才能恢复"的一个已知诱因。
  // 每 10 分钟续一份，远小于常见 TTL。
  const REFRESH_ICE_EVERY_MS = 10 * 60 * 1000;
  let refreshTimer: number | null = null;
  function startRefreshTimer() {
    stopRefreshTimer();
    refreshTimer = window.setInterval(() => {
      if (intentionalClose) return;
      signal.send({ op: "refresh_ice" });
    }, REFRESH_ICE_EVERY_MS);
  }
  function stopRefreshTimer() {
    if (refreshTimer !== null) {
      window.clearInterval(refreshTimer);
      refreshTimer = null;
    }
  }

  const signal = connectSignaling(opts.signalUrl, {
    onOpen() {
      signal.send({
        op: "hello",
        role: "client",
        room: opts.room,
        secret: opts.secret,
      });
      startRefreshTimer();
    },
    onMessage(msg) {
      handleSignal(msg).catch((e) => {
        // 重连竞速下这里仍可能因为别的原因抛错（比如 SDP 解析失败）；吞掉避免
        // unhandled rejection 冒到控制台，让上层重连逻辑按正常失败路径重试。
        console.warn("rtc signal handling failed", e);
      });
    },
    onClose() {
      if (!intentionalClose) fail("signaling closed");
    },
    onError() {
      if (!intentionalClose) fail("signaling error");
    },
  });

  async function handleSignal(msg: SignalingMessage) {
    if (intentionalClose) return;
    switch (msg.op) {
      case "hello_ok": {
        iceServers = iceServersFromHello(msg);
        ensurePc();
        break;
      }
      case "ice_servers": {
        // 只换缓存给以后的 restartIce()/新 pc 用；正在跑的连接不用动，
        // RTCPeerConnection.setConfiguration 对已协商完成的传输不会重新拨号。
        iceServers = iceServersFromHello(msg);
        try {
          pc?.setConfiguration({ iceServers });
        } catch {
          /* 部分浏览器在特定状态下拒绝 setConfiguration，忽略即可，下次 restart 时新 pc 仍会带上新值 */
        }
        break;
      }
      case "peer_joined": {
        if (msg.role === "host") {
          resetPc();
          ensurePc();
          await createAndSendOffer();
        }
        break;
      }
      case "peer_left": {
        if (msg.role === "host") {
          fail("host left");
        }
        break;
      }
      case "signal": {
        if (msg.from !== "host") return;
        await applyRemoteSignal(msg.payload);
        break;
      }
      case "err": {
        fail(msg.msg);
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

  let iceRestarting = false;
  /**
   * 真正的互斥锁，覆盖从"决定要 createOffer"到"setLocalDescription 落地"这一
   * 小段窗口。signalingState 在这段窗口里还是 "stable"（createOffer 本身不
   * 改状态，只有 setLocalDescription 才改），只查 signalingState 挡不住两个
   * 调用方在这段窗口里同时冲进来各发一次 offer——webrtc-internals 实测抓到
   * 过这个场景：两次 createOffer 都成功、两次 setLocalDescription 前后脚
   * 提交，第二次因为跟第一次刚提交的状态对不上直接被 Chrome 拒掉（"order of
   * m-lines ... doesn't match"）。createAndSendOffer（首次协商）和
   * attemptIceRestart（重连）共用这一把锁，谁都不能插队。
   */
  let makingOffer = false;

  /**
   * 网络短暂抖动（弱 WiFi、蜂窝切基站）时优先原地 ICE restart，而不是整个
   * PeerConnection 推倒重建——同一个 pc/DataChannel 都留着，只重新走一轮
   * candidate 收集，代价小、恢复快。8 秒内没连上才退回完整重连（resetPc）。
   */
  async function attemptIceRestart(reason: string) {
    if (intentionalClose || !pc || iceRestarting || makingOffer) return;
    if (pc.signalingState !== "stable") return; // 协商中，等这轮结束再看
    iceRestarting = true;
    setPhase("ice", `restart · ${reason}`);
    try {
      pc.restartIce();
      makingOffer = true;
      let offer: RTCSessionDescriptionInit;
      try {
        offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
      } finally {
        makingOffer = false;
      }
      signal.send({
        op: "signal",
        from: "client",
        payload: { kind: "offer", sdp: offer.sdp || "", restart: true },
      });
      // 故意不在这里把 iceRestarting 重置回 false——offer 刚发出去，answer
      // 还没回来，pc 处于 have-local-offer。如果网络还在抖，别的事件（比如
      // channel 又 close 一次）会紧接着再来一轮 restartOrFail；旧版在这里就
      // 解锁了，一旦这轮 answer 落地、signalingState 瞬间回到 stable，第二个
      // 请求会紧跟着再发一个 offer，跟第一轮还没收尾的 SDP 状态打架，就是
      // Chrome 报 "order of m-lines ... doesn't match" 的根因。等 8 秒超时或
      // 明确成功/失败再放行下一轮。
      window.setTimeout(() => {
        iceRestarting = false;
        if (intentionalClose) return;
        if (pc?.connectionState !== "connected") {
          fail(`ice restart timeout · ${reason}`);
        }
      }, 8000);
    } catch (e) {
      iceRestarting = false;
      // 之前这里吞掉了具体异常，只剩个笼统的 "ice restart failed"，排查全靠猜。
      const detail =
        e instanceof DOMException ? `${e.name}: ${e.message}` : String(e);
      console.warn("ice restart failed", reason, e);
      fail(`ice restart failed(${detail}) · ${reason}`);
    }
  }

  /**
   * DataChannel 关闭（channel.onclose）或其它触发点：能原地重协商就重协商，
   * 不能就直接走完整重连。iceRestarting 已经在飞时什么都不做——那轮自己的
   * 8 秒超时会决定成败，这里再插一脚只会打断它。
   */
  function restartOrFail(reason: string) {
    if (intentionalClose) return;
    if (iceRestarting || makingOffer) return;
    if (pc && pc.signalingState === "stable") {
      void attemptIceRestart(reason);
    } else {
      fail(reason);
    }
  }

  function ensurePc() {
    if (pc) return;
    setPhase("ice");
    pc = new RTCPeerConnection({ iceServers });

    pc.onicecandidate = (ev) => {
      if (intentionalClose) return;
      const payload: SignalPayload = ev.candidate
        ? { kind: "ice", candidate: ev.candidate.toJSON() }
        : { kind: "ice", candidate: null };
      signal.send({ op: "signal", from: "client", payload });
    };

    pc.onconnectionstatechange = () => {
      if (intentionalClose) return;
      const s = pc?.connectionState;
      if (s === "connected") {
        setPhase("connected");
        // 提前解锁：真连上了就不用等满 8 秒 watchdog，后面再抖一次可以马上重协商
        iceRestarting = false;
      } else if (s === "failed") restartOrFail("peer connection failed");
      else if (s === "disconnected") {
        // 换网常见：先 disconnected，稍后可能 failed 或自己恢复；给宽限
        setPhase("ice", "disconnected");
        window.setTimeout(() => {
          if (intentionalClose) return;
          if (pc?.connectionState === "disconnected" || pc?.connectionState === "failed") {
            restartOrFail("peer disconnected");
          }
        }, 4000);
      }
    };

    pc.oniceconnectionstatechange = () => {
      if (intentionalClose) return;
      const s = pc?.iceConnectionState;
      if (s === "failed") restartOrFail("ice failed");
    };

    pc.ondatachannel = (ev) => {
      wireDc(ev.channel);
    };

    const channel = pc.createDataChannel(DC_LABEL, {
      ordered: true,
    });
    wireDc(channel);
  }

  function wireDc(channel: RTCDataChannel) {
    dc = channel;
    channel.binaryType = "arraybuffer";
    channel.onopen = () => {
      if (intentionalClose) return;
      setPhase("connected");
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
        const text = new TextDecoder().decode(ev.data);
        opts.onFrame?.(text);
      }
    };
    channel.onclose = () => {
      // dc 关闭常常是 pc 传输层已经出问题的下游症状（有时比
      // onconnectionstatechange/oniceconnectionstatechange 先触发），走同一套
      // 「能重协商就重协商」逻辑，别一上来就整链路重连。
      if (!intentionalClose && phase !== "closed") {
        restartOrFail("datachannel closed");
      }
    };
  }

  async function createAndSendOffer() {
    if (!pc || makingOffer) return;
    makingOffer = true;
    let offer: RTCSessionDescriptionInit;
    try {
      offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
    } finally {
      makingOffer = false;
    }
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
      // resetPc()/ensurePc() 之后 signalingState 才是 "stable"；重连竞速时可能
      // 收到上一轮协商的过期 offer（新 pc 已经在别的状态了），套用会直接抛
      // InvalidStateError，丢弃即可——host 端很快会因新一轮 peer_joined 重发。
      if (pc.signalingState !== "stable") return;
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
      // 同上：只有当前 pc 确实处于「已发 offer、等 answer」时才认这条 answer，
      // 否则是重连竞速下的过期消息（旧一轮的 answer 追上了新建的 pc）。
      if (pc.signalingState !== "have-local-offer") return;
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

  function sendRaw(raw: string): boolean {
    if (dc && dc.readyState === "open") {
      try {
        dc.send(raw);
        return true;
      } catch {
        return false;
      }
    }
    return false;
  }

  function sendFrame(frame: DcFrame): boolean {
    return sendRaw(encodeFrame(frame));
  }

  function close() {
    intentionalClose = true;
    setPhase("closed");
    stopRefreshTimer();
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
