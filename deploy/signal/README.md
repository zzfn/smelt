# 公网信令部署（腾讯云 Ubuntu）

```text
wss://<你的域名>/ws
```

**VPS 不用装 Rust。** 进程只听 `127.0.0.1:7878`，外网只开 **80/443**。

国内直连 GitHub 常卡：**默认用镜像**（`ghfast.top` / `ghproxy.net` 等）。

| 你怎么上机器 | 看哪份文档 |
|--------------|------------|
| **网页终端 / VNC** | **[`WEB-CONSOLE.md`](./WEB-CONSOLE.md)** ← 镜像一键粘贴 |
| 本机已 `ssh user@ip` | 下文或 `install.sh`（同样默认镜像） |

```bash
# 镜像下载示例（网页终端可直接跑）
curl -fL --connect-timeout 15 --max-time 180 \
  -o /tmp/smelt-signal \
  "https://ghfast.top/https://github.com/smelt-ai/smelt/releases/download/signal-nightly/smelt-signal-x86_64-unknown-linux-gnu"
```

---

## 你需要准备

| 项 | 说明 |
|----|------|
| 腾讯云 CVM | Ubuntu 22.04/24.04 **x86_64**，有公网 IP |
| 域名（上 HTTPS 时） | A 记录 → 公网 IP |
| 安全组 | TCP **80、443**（不要开 7878） |

---

## 推荐：Mac 推包 + 上机安装（快、能看见进度）

### 1）在 Mac 仓库根目录

```bash
chmod +x deploy/signal/push-from-mac.sh deploy/signal/install.sh
./deploy/signal/push-from-mac.sh ubuntu@你的公网IP
```

会在本机拉 `signal-nightly` 二进制并 scp 到 VPS 的 `/tmp/smelt-signal-deploy/`。

### 2）SSH 上机——先只验证进程（不需域名）

```bash
ssh ubuntu@你的公网IP
cd /tmp/smelt-signal-deploy
sudo SKIP_TLS=1 BIN=/tmp/smelt-signal-deploy/smelt-signal bash install.sh
curl -sS http://127.0.0.1:7878/health
# 期望 {"ok":true,"rooms":0}
```

日志形如：`[14:02:11] >>> 步骤 1/6：…`。卡在哪一行就是哪一步。

### 3）有域名后再上 HTTPS（默认 nginx + certbot，走 apt）

```bash
sudo DOMAIN=signal.你的域名.com \
  BIN=/tmp/smelt-signal-deploy/smelt-signal \
  TLS=nginx \
  bash install.sh
```

探活：

```bash
curl -sS https://signal.你的域名.com/health
curl -sS -X POST https://signal.你的域名.com/v1/rooms \
  -H 'content-type: application/json' -d '{}'
```

信令地址：`wss://signal.你的域名.com/ws`

### `install.sh` 环境变量

| 变量 | 含义 |
|------|------|
| `DOMAIN` | 域名；`SKIP_TLS=1` 时可空 |
| `BIN` | 已有二进制路径（强烈建议，避免 VPS 拉 GitHub） |
| `TLS` | `nginx`（默认，国内快）/ `caddy`（国外源易卡）/ `none` |
| `SKIP_TLS` | `1` = 只装进程 + 本机 health |

---

## 升级二进制

在 Mac 再跑一遍 `push-from-mac.sh`，上机：

```bash
sudo install -m 755 /tmp/smelt-signal-deploy/smelt-signal /usr/local/bin/smelt-signal
sudo systemctl restart smelt-signal
```

房间在内存里，重启会清空。

---

## 文件说明

| 文件 | 作用 |
|------|------|
| `push-from-mac.sh` | Mac：下二进制 + scp |
| `install.sh` | VPS：分步安装（超时+日志） |
| `smelt-signal.service` | systemd |
| `smelt-signal.env.example` | → `/etc/smelt/smelt-signal.env` |
| `coturn.md` | TURN 部署说明 |
| `install-coturn.sh` | 同机装 coturn + 写 `SMELT_ICE_SERVERS` |
| `rotate-turn-credential.sh` | 手动轮换 TURN 静态密码（凭证是长期的，建议定期跑） |
| `turnserver.conf.example` | coturn 配置模板 |
| `Caddyfile` | 可选 Caddy |
| `Dockerfile` | 可选容器构建 |
| `.github/workflows/signal.yml` | CI → `signal-nightly` |

---

## 跨网 ICE（STUN / TURN）

- **默认**：进程内置多源公共 STUN（腾讯 / 小米 / Cloudflare / Google），零配置。
- **推荐生产**（蜂窝、严格 NAT）：同机装 **coturn**，信令下发 TURN。  
  → 完整步骤：[`coturn.md`](./coturn.md) · 脚本：[`install-coturn.sh`](./install-coturn.sh)

```bash
sudo PUBLIC_IP=你的弹性公网IP DOMAIN=signal.你的域名.com \
  bash install-coturn.sh
```

## 腾讯云安全组

| 协议 | 端口 | 用途 |
|------|------|------|
| TCP 80 | ACME + HTTP |
| TCP 443 | HTTPS / WSS |
| TCP 22 | SSH（建议收紧来源） |
| UDP/TCP **3478** | coturn STUN/TURN（装了才开） |
| UDP **49152–49251** | coturn 中继（装了才开；可按 conf 调整） |

---

## 排错

| 现象 | 检查 |
|------|------|
| 命令一直无输出 | 多半卡在无超时的 `curl`/`apt`；改用本脚本（有超时） |
| 下载 GitHub 失败 | 用 `push-from-mac.sh`，`BIN=` 指向上传的文件 |
| 本机 health OK、HTTPS 挂 | 安全组 80/443、DNS、`journalctl -u nginx -e` |
| certbot 失败 | 域名是否已指向本机；80 是否通 |

```bash
systemctl status smelt-signal nginx
journalctl -u smelt-signal -n 50 --no-pager
```

---

## 和客户端怎么接（之后）

1. Mac bridge：`POST https://域名/v1/rooms` → `wss://域名/ws` 以 host `hello`
2. 分享链接带 `room` / `k` / `signal`
3. 手机 SPA：`connectRtc({ signalUrl: "wss://域名/ws", ... })`
