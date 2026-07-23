/** 跨网连接状态（设置页 / 顶栏可展示） */

export type RtcConnPhase =
  | "idle"
  | "signaling"
  | "ice"
  | "connected"
  | "relay" // ICE 选中 relay 时可由上层标
  | "reconnecting" // 换网 / 断线后自动重连
  | "failed"
  | "closed";

export type IceServerConfig = {
  urls: string | string[];
  username?: string;
  credential?: string;
};

export type SignalingHello = {
  op: "hello";
  role: "client" | "host";
  room: string;
  secret: string;
};

export type SignalingMessage =
  | SignalingHello
  | { op: "hello_ok"; ice_servers: IceServerConfig[] }
  /** 主动发 refresh_ice 的回应：现算的临时 TURN 凭证过期时间到了要续一份 */
  | { op: "ice_servers"; ice_servers: IceServerConfig[] }
  | { op: "peer_joined"; role: "client" | "host" }
  | { op: "peer_left"; role: "client" | "host" }
  | { op: "signal"; from: "client" | "host"; payload: SignalPayload }
  | { op: "err"; msg: string }
  | { op: "ping" }
  | { op: "pong" }
  | { op: "refresh_ice" };

export type SignalPayload =
  /** restart=true：同一 PeerConnection 上的 ICE 重协商，host 侧不重建 PC/DataChannel */
  | { kind: "offer"; sdp: string; restart?: boolean }
  | { kind: "answer"; sdp: string }
  | { kind: "ice"; candidate: RTCIceCandidateInit | null };

export type RtcConnectOptions = {
  /** wss://signal.example.com/ws */
  signalUrl: string;
  room: string;
  secret: string;
  /** 业务 token（hello 帧带给 bridge） */
  token: string;
  write?: boolean;
  onPhase?: (phase: RtcConnPhase, detail?: string) => void;
  onFrame?: (raw: string) => void;
  /** 非主动 close 时的断线（换网 / PC failed / DC 关） */
  onClose?: (reason: string) => void;
};
