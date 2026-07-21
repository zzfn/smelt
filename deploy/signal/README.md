# 公网信令部署（腾讯云 Ubuntu）

把 `smelt-signal` 跑在 VPS 上，手机和 Mac 都连：

```text
wss://<你的域名>/ws
```

本目录默认方案：**本机二进制 + systemd + Caddy（自动 HTTPS）**。  
进程只听 `127.0.0.1:7878`，外网只开 **80/443**。

---

## 你需要准备

| 项 | 说明 |
|----|------|
| 腾讯云 CVM | Ubuntu 22.04/24.04，有公网 IP |
| **域名** | A 记录指到该公网 IP（Let's Encrypt 必需；纯 IP 不好做正规 WSS） |
| 安全组 | 入站放行 **TCP 80、443**（不必放行 7878） |

DNS 示例：`signal.example.com` → `1.2.3.4`。

---

## 一键脚本（推荐）

在 **VPS** 上：

```bash
# 1) 装依赖（首次）
sudo apt update
sudo apt install -y build-essential pkg-config curl git

# Rust（若还没有）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Caddy（官方源）
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy

# 2) 拉代码并编译（只编信令，不碰 GUI）
git clone <你的 smelt 仓库 URL> ~/smelt
cd ~/smelt
git checkout feat/webrtc-edge   # 或合并后的主分支
cargo build -p smelt-signal --release

# 3) 安装二进制 + systemd
sudo install -m 755 target/release/smelt-signal /usr/local/bin/smelt-signal
sudo mkdir -p /etc/smelt
sudo cp deploy/signal/smelt-signal.env.example /etc/smelt/smelt-signal.env
sudo cp deploy/signal/smelt-signal.service /etc/systemd/system/smelt-signal.service
# 编辑环境文件（通常不用改 bind）
sudo systemctl daemon-reload
sudo systemctl enable --now smelt-signal
curl -sS http://127.0.0.1:7878/health
# 期望：{"ok":true,"rooms":0}

# 4) Caddy 反代 + 自动证书
# 把 Caddyfile 里的域名改成你的
sudo cp deploy/signal/Caddyfile /etc/caddy/Caddyfile
sudo sed -i 's/signal.example.com/你的真实域名/g' /etc/caddy/Caddyfile
sudo systemctl reload caddy

# 5) 公网探活
curl -sS https://你的真实域名/health
```

建房试一下：

```bash
curl -sS -X POST https://你的真实域名/v1/rooms \
  -H 'content-type: application/json' -d '{}'
```

浏览器 / bridge 信令地址：

```text
wss://你的真实域名/ws
```

---

## 文件说明

| 文件 | 作用 |
|------|------|
| `smelt-signal.service` | systemd 单元 |
| `smelt-signal.env.example` | 环境变量模板 → `/etc/smelt/smelt-signal.env` |
| `Caddyfile` | TLS + 反代到 127.0.0.1:7878 |
| `Dockerfile` | 可选：容器构建（小机可先用二进制方案） |

---

## 环境变量

见 `smelt-signal.env.example`：

- `SMELT_SIGNAL_BIND`：生产保持 `127.0.0.1:7878`
- `SMELT_ROOM_TTL_SECS`：房间存活（默认 3600）
- `SMELT_ICE_SERVERS`：JSON；**现阶段可只用公共 STUN**，coturn 以后再加

---

## 腾讯云安全组

控制台 → 云服务器 → 安全组 → 入站规则：

| 协议 | 端口 | 来源 | 用途 |
|------|------|------|------|
| TCP | 80 | 0.0.0.0/0 | ACME HTTP-01 + 跳转 HTTPS |
| TCP | 443 | 0.0.0.0/0 | HTTPS / WSS |
| TCP | 22 | 你的 IP | SSH（按你习惯收紧） |

**不要**把 7878 对公网打开。

---

## 升级

```bash
cd ~/smelt
git pull
cargo build -p smelt-signal --release
sudo install -m 755 target/release/smelt-signal /usr/local/bin/smelt-signal
sudo systemctl restart smelt-signal
```

注意：当前房间在**内存**里，重启会清空进行中的房间。

---

## 排错

| 现象 | 检查 |
|------|------|
| `curl https://域名/health` 失败 | 安全组 80/443、DNS A 记录、`journalctl -u caddy -e` |
| Caddy 证书失败 | 域名是否已解析到本机；80 是否通 |
| health 本机 OK、外网不通 | 是否只绑了 7878 却没配 Caddy；安全组 |
| WSS 连不上 | 必须用 `wss://`（不是 `ws://`）；证书是否有效 |

```bash
sudo systemctl status smelt-signal caddy
journalctl -u smelt-signal -e --no-pager | tail -50
journalctl -u caddy -e --no-pager | tail -50
```

---

## 和 smelt 客户端怎么接（之后）

1. Mac `smelt-bridge`：`POST https://域名/v1/rooms` → 拿 `room`/`secret`，再 `wss://域名/ws` 以 **host** 身份 `hello`
2. 分享链接：`...?room=...&k=...&signal=wss://域名/ws`
3. 手机 SPA：`connectRtc({ signalUrl: "wss://域名/ws", ... })`

coturn 未上线前，同网/宽松 NAT 仍可能 P2P 成功；跨运营商失败时再上 TURN。
