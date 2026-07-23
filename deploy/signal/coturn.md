# coturn（STUN/TURN）— 跨网「真可用」

smelt 跨网默认只带**公共 STUN**（腾讯 / 小米 / Cloudflare / Google）。  
大约 7～8 成家宽/宽松 NAT 能直连；**手机蜂窝、对称 NAT** 往往必须 **TURN 中继**。

本页：在**已有 smelt-signal 的腾讯云 Ubuntu** 上装 coturn，并让信令通过 `hello_ok` 下发 TURN。

```text
手机 ──WSS──► smelt-signal（:443）
  │                │
  │   ICE 列表含 turn:你的IP/域名:3478（临时凭证，现算现发）
  │                │
  └──DataChannel──►（能直连则 P2P；否则经 coturn 中继）──► Mac smelt-bridge
```

**凭证模式**：用 coturn 的 REST API 临时凭证（`use-auth-secret`），不是固定
用户名密码。`smelt-signal` 和 `coturn` 之间只共享**一份密钥**（从不下发给
客户端），`smelt-signal` 每次 `hello_ok` 用这份密钥现算一个几小时后过期的
临时用户名/密码追加进 ICE 列表。比旧版直接把 TURN 密码写死发给所有人更安全
（凭证会过期），也不存在"两个配置文件的密码必须人肉保持一致"这件事——早期
版本用的是固定密码（`lt-cred-mech`），真出过一次两边密码手动改分叉、两个
服务都正常启动没有任何报错、只是所有人都连不上的事故，才换成这个方案。

---

## 一分钟结论

| 层级 | 配置 | 作用 |
|------|------|------|
| 默认 | 代码内 4 个公共 STUN | 零运维，多数 Wi‑Fi 够用 |
| **推荐生产** | 同机 coturn（REST API 临时凭证） | 蜂窝 / 严格 NAT 也能通，且凭证不会永久有效 |

---

## 安全组（必做）

在腾讯云控制台给这台 CVM 加：

| 协议 | 端口 | 用途 |
|------|------|------|
| UDP | **3478** | STUN/TURN |
| TCP | **3478** | TURN over TCP（UDP 被墙时） |
| UDP | **49152–49251** | 中继媒体（脚本默认区间；可改） |

原有 **TCP 80/443** 保持不变。不要把 7878 暴露公网。

---

## 推荐：一键脚本

仓库文件：[`install-coturn.sh`](./install-coturn.sh)

### 1）把脚本弄上机

Mac：

```bash
scp deploy/signal/install-coturn.sh ubuntu@你的公网IP:/tmp/
```

或网页终端里 `curl` 仓库 raw / 粘贴脚本内容。

### 2）执行

```bash
# 有域名（推荐，与 signal 同域即可）
sudo PUBLIC_IP=你的弹性公网IP \
  DOMAIN=signal.你的域名.com \
  bash /tmp/install-coturn.sh

# 只有 IP
sudo PUBLIC_IP=你的弹性公网IP bash /tmp/install-coturn.sh
```

脚本会：

1. `apt install coturn`
2. 写 `/etc/turnserver.conf`（`use-auth-secret` 模式，随机密钥，结束时打印一次）
3. 启用并 restart coturn
4. 往 `/etc/smelt/smelt-signal.env` 写入 `SMELT_TURN_SECRET` / `SMELT_TURN_HOST`
5. `systemctl restart smelt-signal`

**请保存打印出的 secret**（这不是发给客户端的密码，是服务端两边共享的密钥，
丢了只能重新生成一份，不影响已经在用的凭证之外没有其它后果）。

### 3）验证

```bash
# coturn 在听
ss -ulnp | grep 3478
systemctl status coturn   # 部分镜像 unit 名是 turnserver

# 信令环境
grep SMELT_TURN /etc/smelt/smelt-signal.env
systemctl restart smelt-signal
journalctl -u smelt-signal -n 20 --no-pager | grep turn_rest   # 应该是 turn_rest=true
```

Mac 上**关掉再开**「跨网 WebRTC」（换新房间），手机重新扫码。  
浏览器开发者工具 / `chrome://webrtc-internals` 里，蜂窝场景下应能看到 **relay** 候选。

---

## 手动配置（不用脚本时）

### 1）安装

```bash
sudo apt-get update
sudo apt-get install -y coturn
```

### 2）配置

参考 [`turnserver.conf.example`](./turnserver.conf.example)，复制为 `/etc/turnserver.conf`：

- `external-ip=` **弹性公网 IP**
- `use-auth-secret` + `static-auth-secret=一串随机密钥`（比如 `openssl rand -base64 32`）
- `realm=` 域名或自拟
- `min-port` / `max-port` 与安全组一致

`/etc/default/coturn`：

```bash
TURNSERVER_ENABLED=1
```

```bash
sudo systemctl enable --now coturn
```

### 3）信令下发 ICE

`/etc/smelt/smelt-signal.env`（**单行、尽量无空格**；systemd `EnvironmentFile` 对引号敏感）：

```bash
SMELT_TURN_SECRET=你的static-auth-secret
SMELT_TURN_HOST=signal.example.com:3478
```

`SMELT_TURN_SECRET` 必须跟 `turnserver.conf` 里的 `static-auth-secret=`
**逐字符一致**——这是唯一要跨两个文件保持同步的地方，改的时候两边一起改，
改完都要重启（coturn + smelt-signal）。然后：

```bash
sudo systemctl restart smelt-signal
```

没设 `SMELT_TURN_SECRET` 时进程会打 `turn_rest=false`（见启动日志），只发
`SMELT_ICE_SERVERS` 里配的 STUN，不会报错也不会阻塞启动。

---

## 可选：TURNS（TLS 5349）

已有 Let’s Encrypt（signal 同域名）时，在 `turnserver.conf` 增加：

```text
tls-listening-port=5349
cert=/etc/letsencrypt/live/你的域名/fullchain.pem
pkey=/etc/letsencrypt/live/你的域名/privkey.pem
```

安全组再放行 **TCP 5349**。目前 `smelt-signal` 现算的 ICE 列表只带
`turn:...?transport=udp/tcp`，不含 `turns:`；要用 TURNS 需要在
`crates/smelt-signal/src/state.rs` 的 `ice_servers_for_hello` 里手动加一条
`turns:你的域名:5349?transport=tcp`。

证书续期后需保证 coturn 能读新文件（或 reload）。

---

## 排错

| 现象 | 检查 |
|------|------|
| 仍只有 host/srflx、无 relay | 安全组 UDP 中继段？`external-ip` 是否公网 IP？`SMELT_TURN_SECRET` 是否两边一致？ |
| 本机 3478 没在听 | `TURNSERVER_ENABLED=1`、`journalctl -u coturn` |
| 信令 hello 仍无 turn | `journalctl -u smelt-signal` 启动日志里 `turn_rest` 是不是 `true`；`SMELT_TURN_SECRET`/`SMELT_TURN_HOST` 有没有生效（`systemctl restart smelt-signal` 过没有） |
| relay 候选建立了但连不通 / 中途断 | 检查 `SMELT_TURN_TTL_SECS` 是不是设得太短，长会话中途 ICE restart 时凭证过期会被拒 |
| 带宽暴涨 | 大量会话在中继；DataChannel 文本一般不大，查是否异常重连 |

在线探测（本机或其它机器，需要先拿一份当前有效的临时凭证——没有现成 CLI 工具直接生成，
最简单是从 `chrome://webrtc-internals` 或 `journalctl` 里抓一次真实握手的 username/credential）：

```bash
turnutils_uclient -v -u 抓到的username -w '抓到的credential' 你的公网IP
```

---

## 和「只改公共 STUN」的关系

- **不装 coturn**：客户端仍用内置 4 个公共 STUN（代码默认），零成本。
- **装了 coturn**：ICE 列表在公共 STUN 之外，追加一条现算的临时 TURN。
- 轮换共享密钥：见下「密钥轮换」。

---

## 密钥轮换（可选，不再是"堵漏"必须操作）

旧版 `lt-cred-mech` 固定凭证没有过期时间，`hello_ok` 把它发给**每一个**连上
信令的客户端，泄露了就能被当免费中继一直用到手动换密码为止——这也是当初
做 REST API 临时凭证的原因。**换了这套方案之后，凭证本身已经会自动过期**，
轮换共享密钥不再是防泄露的唯一手段，纯粹是防御纵深（怀疑密钥被看到过、
定期例行操作），不跑也不影响正常使用：

```bash
sudo bash rotate-turn-credential.sh
# 或指定新密钥：
sudo TURN_SECRET=新密钥 bash rotate-turn-credential.sh
```

会原地更新 `/etc/turnserver.conf` 的 `static-auth-secret=` 与
`/etc/smelt/smelt-signal.env` 的 `SMELT_TURN_SECRET`，重启 coturn +
smelt-signal。旧密钥算出来的临时凭证立即失效，正在用的中继连接下次
重连/ICE restart 会失败（重新走一遍 hello 就能拿到新凭证）。

更多协议背景见 [`docs/webrtc-edge.md`](../../docs/webrtc-edge.md)。
