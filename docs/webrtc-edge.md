# 跨网远程：WebRTC + 自营 TURN（已定稿）

状态：**方案已冻结，待 W0 Spike / 实现。**  
取代：Quick Tunnel 作为跨网主路径（仍可作高级临时选项）。  
相关：`docs/collaboration.md`（早期 P2P 讨论）、`docs/remote-ops-roadmap.md` Phase 3（原 CF 决策，实现后需回写）。

---

## 一句话

**信令只握手；数据优先手机↔Mac 点对点；打洞失败才走自营 TURN 中继。**  
业务仍是本机 `remote_gateway` + smeltd；浏览器零安装。

---

## 架构

```
手机浏览器 (remote-web + RTCPeerConnection)
    │ WSS 信令（SDP/ICE）          │ DataChannel（PTY/控制帧）
    ▼                              ▼
公网 VPS: signal + coturn     ◄──►  Mac: smelt-bridge
                                       │ 本机 HTTP/unix
                                       ▼
                                 remote_gateway / smeltd
```

| 组件 | 碰业务数据？ |
|------|----------------|
| 信令 | 否 |
| STUN | 否 |
| P2P 直连 | 是（不经 VPS） |
| TURN | **仅兜底时**是 |
| Bridge | 是（桥到本机） |

---

## 已拍板

| 项 | 决定 |
|----|------|
| 跨网主路径 | 自建信令 + WebRTC DC + 自营 TURN |
| Bridge | **独立进程** `smelt-bridge`（或 `smelt-edge`） |
| DC 协议 | 紧凑帧 α（hello/sessions/open/pty/input/action/resize/…） |
| 接收端 | 浏览器 |
| CF Quick | 高级/不稳定，文案降权 |
| Tailscale | 文档备选，不强推 |

---

## 数据面帧（MVP 草案）

```
hello | sessions | sessions_ok | open | pty | input | action | resize | state | err
```

手机侧：`HttpTransport`（局域网）与 `RtcTransport`（跨网）统一上层 UI。

---

## 信令房间（MVP）

- Mac 创建 `room_id` + 高熵 `secret`，短时效  
- 分享链接带 room + secret  
- 信令只转发 signal 消息，不转发 PTY  

---

## 分期

| 阶段 | 内容 |
|------|------|
| **W0 Spike** | 库选型、浏览器↔Rust DC、STUN/TURN 双路径、1h 资源粗测 |
| **W1** | 信令 + bridge + SPA RtcTransport + 设置「跨网链接」 |
| **W2** | TURN 限时凭证、房间过期、质量指示、文档/roadmap 回写 |
| **W3** | 应用层加密、多观看者、原生推送（可选） |

**W0 Gate：** 蜂窝↔家宽可连通（含强制 TURN 路径）；资源无失控。不过 gate 不进 W1。

---

## 设置页推荐序

1. 本机 / 局域网  
2. **跨网（WebRTC）** ← 主路径  
3. 临时 Cloudflare（高级）  

---

## 成功标准

1. 跨网成功率显著高于 Quick Tunnel  
2. 手机仅浏览器  
3. 日志可区分：信令 / ICE / TURN / 本机 gateway  
4. 未开跨网时不建房间、不乱监听  

---

## 前置依赖

- 公网 VPS + 域名 TLS（signal + coturn）  
- 无 VPS 则无法交付 W1 跨网主路径（局域网 HTTP 仍可用）  

---

## 浏览器实现选型（已定）

| 层 | 选型 |
|----|------|
| 手机 WebRTC | **原生** `RTCPeerConnection` + `RTCDataChannel`（不引入 PeerJS / simple-peer） |
| 信令 | 原生 `WebSocket`（`remote-web/src/transport/signaling.ts`） |
| Mac Bridge | 后续 `webrtc-rs`（W0 spike） |

### 前端脚手架（`feat/webrtc-edge`）

```
remote-web/src/transport/
  types.ts       # 连接状态、ICE、信令消息
  frames.ts      # DC 业务帧编解码 + base64
  signaling.ts   # WSS 信令客户端
  rtc-peer.ts    # 原生 PC + DC；parseRtcQuery
  index.ts
```

局域网路径仍用现有 `api.ts` fetch/WS；跨网就绪后 UI 按 `?room=&k=&signal=` 走 `connectRtc`。

### 信令线协议（与 `signaling.ts` 对齐）

```json
{ "op": "hello", "role": "client"|"host", "room": "...", "secret": "..." }
{ "op": "hello_ok", "ice_servers": [{ "urls": "...", "username?", "credential?" }] }
{ "op": "peer_joined", "role": "client"|"host" }
{ "op": "signal", "from": "client"|"host", "payload": { "kind": "offer"|"answer"|"ice", ... } }
{ "op": "err", "msg": "..." }
{ "op": "ping" } / { "op": "pong" }
```

### 信令服务（`crates/smelt-signal`，已实现）

```bash
cargo run -p smelt-signal
# 默认 bind 127.0.0.1:7878
```

| 端点 | 作用 |
|------|------|
| `GET /health` | `{ ok, rooms }` |
| `POST /v1/rooms` | body 可选 `{ "ttl_secs" }` → `{ room, secret, expires_at, ttl_secs, signal_ws }` |
| `GET /ws` | 信令 WebSocket（上表协议） |

环境变量：

| 变量 | 默认 | 含义 |
|------|------|------|
| `SMELT_SIGNAL_BIND` | `127.0.0.1:7878` | 监听地址 |
| `SMELT_ROOM_TTL_SECS` | `3600` | 房间默认存活 |
| `SMELT_ICE_SERVERS` | Google 公共 STUN | JSON 数组，下发给 `hello_ok` |

本地 dev 只给 STUN；生产把 coturn 写进 `SMELT_ICE_SERVERS`。房间内存存、短时效，进程重启清空。

**公网部署（腾讯云 Ubuntu 等）：** 见 [`deploy/signal/README.md`](../deploy/signal/README.md)  
（二进制 + systemd + Caddy → `https://域名/health`、`wss://域名/ws`）。

### DataChannel 标签

- label: `smelt`
- ordered: true  
