# coturn（STUN/TURN）— 跨网「真可用」

smelt 跨网默认只带**公共 STUN**（腾讯 / 小米 / Cloudflare / Google）。  
大约 7～8 成家宽/宽松 NAT 能直连；**手机蜂窝、对称 NAT** 往往必须 **TURN 中继**。

本页：在**已有 smelt-signal 的腾讯云 Ubuntu** 上装 coturn，并让信令通过 `hello_ok` 下发 TURN。

```text
手机 ──WSS──► smelt-signal（:443）
  │                │
  │   ICE 列表含 turn:你的IP/域名:3478
  │                │
  └──DataChannel──►（能直连则 P2P；否则经 coturn 中继）──► Mac smelt-bridge
```

---

## 一分钟结论

| 层级 | 配置 | 作用 |
|------|------|------|
| 默认 | 代码内 4 个公共 STUN | 零运维，多数 Wi‑Fi 够用 |
| **推荐生产** | 同机 coturn + `SMELT_ICE_SERVERS` | 蜂窝 / 严格 NAT 也能通 |

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
2. 写 `/etc/turnserver.conf`（随机 TURN 密码，结束时打印一次）
3. 启用并 restart coturn
4. 往 `/etc/smelt/smelt-signal.env` 写入 `SMELT_ICE_SERVERS=...`（本机 TURN + 公共 STUN）
5. `systemctl restart smelt-signal`

**请保存打印出的 TURN 密码。**

### 3）验证

```bash
# coturn 在听
ss -ulnp | grep 3478
systemctl status coturn   # 部分镜像 unit 名是 turnserver

# 信令环境
grep SMELT_ICE_SERVERS /etc/smelt/smelt-signal.env
systemctl restart smelt-signal
journalctl -u smelt-signal -n 20 --no-pager
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
- `user=smelt:强密码`
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
SMELT_ICE_SERVERS=[{"urls":"stun:signal.example.com:3478"},{"urls":["turn:signal.example.com:3478?transport=udp","turn:signal.example.com:3478?transport=tcp"],"username":"smelt","credential":"你的密码"},{"urls":"stun:stun.qq.com:3478"},{"urls":"stun:stun.miwifi.com:3478"},{"urls":"stun:stun.cloudflare.com:3478"},{"urls":"stun:stun.l.google.com:19302"}]
```

把域名/密码换成你的。然后：

```bash
sudo systemctl restart smelt-signal
```

`SMELT_ICE_SERVERS` 解析失败时，进程会打 warn 并回退到内置公共 STUN。

---

## 可选：TURNS（TLS 5349）

已有 Let’s Encrypt（signal 同域名）时，在 `turnserver.conf` 增加：

```text
tls-listening-port=5349
cert=/etc/letsencrypt/live/你的域名/fullchain.pem
pkey=/etc/letsencrypt/live/你的域名/privkey.pem
```

安全组再放行 **TCP 5349**，ICE 增加：

```json
"turns:你的域名:5349?transport=tcp"
```

证书续期后需保证 coturn 能读新文件（或 reload）。

---

## 排错

| 现象 | 检查 |
|------|------|
| 仍只有 host/srflx、无 relay | 安全组 UDP 中继段？`external-ip` 是否公网 IP？ |
| 本机 3478 没在听 | `TURNSERVER_ENABLED=1`、`journalctl -u coturn` |
| 信令 hello 仍无 turn | `grep SMELT_ICE_SERVERS`、是否 restart signal、JSON 是否被 env 截断 |
| 带宽暴涨 | 大量会话在中继；DataChannel 文本一般不大，查是否异常重连 |

在线探测（本机或其它机器）：

```bash
# 需安装 coturn 自带的客户端工具，或使用浏览器 webrtc-internals
turnutils_uclient -v -u smelt -w '你的密码' 你的公网IP
```

---

## 和「只改公共 STUN」的关系

- **不装 coturn**：客户端仍用内置 4 个公共 STUN（代码默认），零成本。
- **装了 coturn**：`SMELT_ICE_SERVERS` 优先下发本机 STUN/TURN，公共 STUN 作冗余。
- 改密码 / 轮换：见下「凭证轮换」，或手动改 `turnserver.conf` 的 `user=` 与 env 里 `credential`，两边 restart。

---

## 凭证轮换（重要）

`lt-cred-mech` 是**静态长期凭证**，没有过期时间。这组用户名密码会通过 `hello_ok`
发给**每一个**连上信令的 WebRTC 客户端（建房接口本身无鉴权，只有全局限流）——
也就是说任何能连到你信令服务器的人，握手一次就能拿到这组凭证，之后完全绕开
smelt 的房间逻辑，把它当一个免费公开的 UDP/TCP 中继一直用到你换密码为止。

没时间接 coturn REST API 临时凭证（真正的修法）时，退而求其次是**定期手动轮换**：

```bash
sudo bash rotate-turn-credential.sh
# 或指定新密码：
sudo TURN_PASS=新密码 bash rotate-turn-credential.sh
```

会原地更新 `/etc/turnserver.conf` 的 `user=` 与 `/etc/smelt/smelt-signal.env` 的
`SMELT_ICE_SERVERS`（ICE host 不变），重启 coturn + smelt-signal，旧凭证立即失效。

**这只是把"泄露后能一直白嫖"收窄成"泄露后能用到下次轮换"，不是真正的修复**——
没人跑这个脚本的话风险敞口和不轮换没区别，建议自己包一个 cron 定期跑。

更多协议背景见 [`docs/webrtc-edge.md`](../../docs/webrtc-edge.md)。
