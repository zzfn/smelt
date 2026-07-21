# 公网信令部署（腾讯云 Ubuntu）

```text
wss://<你的域名>/ws
```

**VPS 不用装 Rust。** 进程只听 `127.0.0.1:7878`，外网只开 **80/443**。

国内机访问 GitHub / Caddy 源经常**无进度卡住**。

| 你怎么上机器 | 看哪份文档 |
|--------------|------------|
| **网页终端 / VNC**（没配 SSH） | **[`WEB-CONSOLE.md`](./WEB-CONSOLE.md)** ← 先看这个 |
| 本机已 `ssh user@ip` | 下文「Mac 推包 + scp」 |

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
| `Caddyfile` | 可选 Caddy |
| `Dockerfile` | 可选容器构建 |
| `.github/workflows/signal.yml` | CI → `signal-nightly` |

---

## 腾讯云安全组

| 协议 | 端口 | 用途 |
|------|------|------|
| TCP 80 | ACME + HTTP |
| TCP 443 | HTTPS / WSS |
| TCP 22 | SSH（建议收紧来源） |

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
