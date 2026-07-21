# 公网信令部署（腾讯云 Ubuntu）

把 `smelt-signal` 跑在 VPS 上，手机和 Mac 都连：

```text
wss://<你的域名>/ws
```

**推荐：CI 打好的 Linux 二进制 + systemd + Caddy**（VPS **不用装 Rust**）。  
进程只听 `127.0.0.1:7878`，外网只开 **80/443**。

---

## 你需要准备

| 项 | 说明 |
|----|------|
| 腾讯云 CVM | Ubuntu 22.04/24.04 **x86_64**，有公网 IP |
| **域名** | A 记录指到该公网 IP（Let's Encrypt 必需；纯 IP 不好做正规 WSS） |
| 安全组 | 入站放行 **TCP 80、443**（不必放行 7878） |

DNS 示例：`signal.example.com` → `1.2.3.4`。

---

## 推荐：下载 GitHub Actions 产物

CI 工作流：`.github/workflows/signal.yml`  
每次 push 信令相关改动（或手动 Run workflow）会：

1. 在 `ubuntu-latest` 编译 `smelt-signal`
2. 发布到滚动 pre-release 标签 **`signal-nightly`**

### 固定下载 URL

把 `OWNER/REPO` 换成实际仓库（例如 `smelt-ai/smelt`）：

```bash
REPO=smelt-ai/smelt
BASE="https://github.com/${REPO}/releases/download/signal-nightly"
curl -fsSL -o smelt-signal -L \
  "${BASE}/smelt-signal-x86_64-unknown-linux-gnu"
curl -fsSL -o smelt-signal.sha256 -L \
  "${BASE}/smelt-signal-x86_64-unknown-linux-gnu.sha256"
# 可选校验
sha256sum -c smelt-signal.sha256
chmod +x smelt-signal
sudo install -m 755 smelt-signal /usr/local/bin/smelt-signal
```

> 仓库若是 **private**：公开 URL 会 404，改用  
> `gh release download signal-nightly -R OWNER/REPO -p 'smelt-signal*'`  
>（需 `gh auth login` 或 `GH_TOKEN`）。

### VPS 首次部署（无 Rust）

```bash
sudo apt update
sudo apt install -y curl ca-certificates

# 1) 二进制（见上）
REPO=smelt-ai/smelt
BASE="https://github.com/${REPO}/releases/download/signal-nightly"
curl -fsSL -o /tmp/smelt-signal -L \
  "${BASE}/smelt-signal-x86_64-unknown-linux-gnu"
sudo install -m 755 /tmp/smelt-signal /usr/local/bin/smelt-signal

# 2) 配置文件：可只 clone 本目录，或从仓库 curl 原材料
#    若已 git clone：
#    cd ~/smelt && git checkout feat/webrtc-edge
sudo mkdir -p /etc/smelt
sudo curl -fsSL -o /etc/smelt/smelt-signal.env \
  "https://raw.githubusercontent.com/${REPO}/feat/webrtc-edge/deploy/signal/smelt-signal.env.example"
sudo curl -fsSL -o /etc/systemd/system/smelt-signal.service \
  "https://raw.githubusercontent.com/${REPO}/feat/webrtc-edge/deploy/signal/smelt-signal.service"

sudo systemctl daemon-reload
sudo systemctl enable --now smelt-signal
curl -sS http://127.0.0.1:7878/health
# 期望：{"ok":true,"rooms":0}

# 3) Caddy
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy

sudo curl -fsSL -o /etc/caddy/Caddyfile \
  "https://raw.githubusercontent.com/${REPO}/feat/webrtc-edge/deploy/signal/Caddyfile"
sudo sed -i 's/signal.example.com/你的真实域名/g' /etc/caddy/Caddyfile
sudo systemctl reload caddy

curl -sS https://你的真实域名/health
```

### 升级二进制

```bash
REPO=smelt-ai/smelt
BASE="https://github.com/${REPO}/releases/download/signal-nightly"
curl -fsSL -o /tmp/smelt-signal -L \
  "${BASE}/smelt-signal-x86_64-unknown-linux-gnu"
sudo install -m 755 /tmp/smelt-signal /usr/local/bin/smelt-signal
sudo systemctl restart smelt-signal
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

## 备选：在 VPS 上自己编译

国内机房访问 `sh.rustup.rs` / crates.io 常很慢，需 [rsproxy](https://rsproxy.cn/) 等镜像。  
一般 **不推荐**，优先用上面的 CI 二进制。

```bash
# 装好 rustup + cargo 镜像后
git clone -b feat/webrtc-edge <repo> ~/smelt && cd ~/smelt
cargo build -p smelt-signal --release
sudo install -m 755 target/release/smelt-signal /usr/local/bin/smelt-signal
```

---

## 文件说明

| 文件 | 作用 |
|------|------|
| `smelt-signal.service` | systemd 单元 |
| `smelt-signal.env.example` | 环境变量模板 → `/etc/smelt/smelt-signal.env` |
| `Caddyfile` | TLS + 反代到 127.0.0.1:7878 |
| `Dockerfile` | 可选：容器构建 |
| `../.github/workflows/signal.yml` | CI 打 Linux 二进制 → `signal-nightly` |

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
