/** 跨网连接状态（设置页 / 顶栏可展示） */

export type RtcConnPhase =
  | "idle"
  | "signaling"
  | "ice"
  | "connected"
  | "relay" // ICE 选中 relay 时可由上层标
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
  | { op: "peer_joined"; role: "client" | "host" }
  | { op: "peer_left"; role: "client" | "host" }
  | { op: "signal"; from: "client" | "host"; payload: SignalPayload }
  | { op: "err"; msg: string }
  | { op: "ping" }
  | { op: "pong" };

export type SignalPayload =
  | { kind: "offer"; sdp: string }
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
  onClose?: (reason: string) => void;
};
