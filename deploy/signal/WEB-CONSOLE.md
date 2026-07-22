# 腾讯云网页终端部署（推荐 GitHub 镜像）

适合：控制台网页终端 / VNC，**不必 SSH、不必 Mac 上传**。

国内直连 `github.com` 常卡住；用 **镜像前缀** 即可在 VPS 上下二进制。

---

## 最快：网页终端一整段（镜像拉二进制）

下面整段复制执行。默认镜像 `https://ghfast.top/`；失败会自动试 `ghproxy.net` 等。

```bash
set -e
# 可选：换镜像  export GH_MIRROR=https://ghproxy.net/
# 可选：直连    export GH_MIRROR=
export GH_MIRROR="${GH_MIRROR-https://ghfast.top/}"

BIN_URL="https://github.com/smelt-ai/smelt/releases/download/signal-nightly/smelt-signal-x86_64-unknown-linux-gnu"
MIRROR_URL="${GH_MIRROR}${BIN_URL}"

echo "[$(date +%H:%M:%S)] 1 下载二进制（镜像）"
echo "    $MIRROR_URL"
curl -fL --connect-timeout 15 --max-time 180 -o /tmp/smelt-signal "$MIRROR_URL" \
  || curl -fL --connect-timeout 15 --max-time 180 -o /tmp/smelt-signal "https://ghproxy.net/${BIN_URL}" \
  || curl -fL --connect-timeout 15 --max-time 180 -o /tmp/smelt-signal "https://mirror.ghproxy.com/${BIN_URL}"
chmod +x /tmp/smelt-signal
ls -lh /tmp/smelt-signal

echo "[$(date +%H:%M:%S)] 2 安装"
sudo install -m 755 /tmp/smelt-signal /usr/local/bin/smelt-signal

echo "[$(date +%H:%M:%S)] 3 配置 + systemd"
sudo mkdir -p /etc/smelt
sudo tee /etc/smelt/smelt-signal.env >/dev/null <<'EOF'
SMELT_SIGNAL_BIND=127.0.0.1:7878
SMELT_ROOM_TTL_SECS=3600
RUST_LOG=info
EOF

sudo tee /etc/systemd/system/smelt-signal.service >/dev/null <<'EOF'
[Unit]
Description=smelt WebRTC signaling (no PTY)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=-/etc/smelt/smelt-signal.env
ExecStart=/usr/local/bin/smelt-signal
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now smelt-signal
sleep 1
echo "[$(date +%H:%M:%S)] 4 health"
curl -sS --connect-timeout 3 --max-time 5 http://127.0.0.1:7878/health
echo
echo "[$(date +%H:%M:%S)] 完成。应看到 {\"ok\":true,\"rooms\":0}"
```

若第 1 步三个镜像都失败，再试：

```bash
export GH_MIRROR=https://ghproxy.net/
# 或
export GH_MIRROR=https://mirror.ghproxy.com/
# 重跑上面下载那几行
```

常用镜像前缀（拼在 **完整 GitHub URL 前面**）：

| 前缀 | 示例 |
|------|------|
| `https://ghfast.top/` | `https://ghfast.top/https://github.com/smelt-ai/smelt/releases/download/...` |
| `https://ghproxy.net/` | 同上 |
| `https://mirror.ghproxy.com/` | 同上 |

镜像站会变动，一个不行换另一个即可。

---

## 有域名：HTTPS（nginx + certbot，apt，不经 GitHub）

安全组 **80、443**；域名 A 记录到位。

```bash
set -e
DOMAIN=signal.你的域名.com   # 改

echo "[$(date +%H:%M:%S)] apt nginx certbot"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -y
sudo apt-get install -y nginx certbot python3-certbot-nginx

sudo tee /etc/nginx/sites-available/smelt-signal >/dev/null <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name ${DOMAIN};
    location / {
        proxy_pass http://127.0.0.1:7878;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
EOF
sudo ln -sfn /etc/nginx/sites-available/smelt-signal /etc/nginx/sites-enabled/smelt-signal
sudo rm -f /etc/nginx/sites-enabled/default
sudo nginx -t && sudo systemctl enable --now nginx && sudo systemctl reload nginx

echo "[$(date +%H:%M:%S)] certbot"
sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos \
  --register-unsafely-without-email --redirect

curl -sS --connect-timeout 10 --max-time 20 "https://${DOMAIN}/health"
echo
echo "WSS: wss://${DOMAIN}/ws"
```

---

## 备选：用 install.sh（也默认镜像）

若机器上已有仓库或能镜像拉 raw：

```bash
# 镜像拉 install.sh
curl -fL --connect-timeout 15 --max-time 60 \
  -o /tmp/install.sh \
  "https://ghfast.top/https://raw.githubusercontent.com/smelt-ai/smelt/feat/webrtc-edge/deploy/signal/install.sh"
sudo SKIP_TLS=1 bash /tmp/install.sh
```

---

## 备选：Mac 上传 / COS

镜像全挂时：Mac 下好 → 控制台上传 / COS，见旧说明；或 `BIN=/tmp/smelt-signal` 再装。

---

## 升级（含 SPA）

CI 会把 **remote-web** 编进 `smelt-signal`。升级：

```bash
export GH_MIRROR="${GH_MIRROR-https://ghfast.top/}"
BIN_URL="https://github.com/smelt-ai/smelt/releases/download/signal-nightly/smelt-signal-x86_64-unknown-linux-gnu"
curl -fL --connect-timeout 15 --max-time 180 -o /tmp/smelt-signal "${GH_MIRROR}${BIN_URL}" \
  || curl -fL --connect-timeout 15 --max-time 180 -o /tmp/smelt-signal "https://ghproxy.net/${BIN_URL}"
sudo install -m 755 /tmp/smelt-signal /usr/local/bin/smelt-signal
sudo systemctl restart smelt-signal
curl -sS https://signal.你的域名/ | head
# 应是 SPA html，不是 404
```

nginx 继续整站反代 `signal` → `127.0.0.1:7878` 即可（`/ws` `/v1` `/health` 与 SPA 同一进程）。

---

## 可选：coturn（手机蜂窝也能连）

安全组先放行：**UDP/TCP 3478**、**UDP 49152–49251**。

```bash
# 镜像拉脚本
curl -fL --connect-timeout 15 --max-time 60 \
  -o /tmp/install-coturn.sh \
  "https://ghfast.top/https://raw.githubusercontent.com/smelt-ai/smelt/feat/webrtc-edge/deploy/signal/install-coturn.sh"

sudo PUBLIC_IP=你的弹性公网IP \
  DOMAIN=signal.你的域名.com \
  bash /tmp/install-coturn.sh
# 结束会打印 TURN 密码，请保存
```

细节与排错：仓库 [`coturn.md`](./coturn.md)。

---

## 排错

| 现象 | 处理 |
|------|------|
| curl 镜像超时 | 换 `GH_MIRROR=https://ghproxy.net/` |
| 下载了但不是二进制（很小/HTML） | 镜像返回错误页；换镜像或看 `file /tmp/smelt-signal` |
| health 失败 | `journalctl -u smelt-signal -n 40 --no-pager` |
| 打开 `/` 不是 SPA | 二进制过旧未嵌 SPA；拉最新 `signal-nightly` |
