# 不用 SSH：腾讯云网页终端 / VNC 部署

适合：控制台「登录」网页终端、VNC、堡垒机网页，**本机没有配置 ssh/scp**。

思路：**GitHub 下载只在 Mac 上做**；二进制用控制台「上传文件」丢进机器；网页终端里只跑本地命令（不访问 GitHub）。

---

## 总流程

```text
Mac 下载 smelt-signal 二进制
    ↓
腾讯云控制台 → 上传到 VPS（如 /tmp/）
    ↓
网页终端：写配置 + 启动（下面复制粘贴）
```

---

## 1. 在 Mac 上下载二进制

浏览器打开（或终端）：

```bash
# Mac 终端
cd ~/Downloads
curl -fL --connect-timeout 15 --max-time 180 \
  -o smelt-signal-linux \
  "https://github.com/smelt-ai/smelt/releases/download/signal-nightly/smelt-signal-x86_64-unknown-linux-gnu"
ls -lh smelt-signal-linux
```

得到文件：`~/Downloads/smelt-signal-linux`（大约几 MB）。

---

## 2. 上传到腾讯云

按你实际登录方式选一种：

### A. 腾讯云「标准登录 / 登录助手」网页终端

部分产品顶部有 **上传文件** / 文件夹图标 → 选 `smelt-signal-linux` → 传到当前用户家目录或 `/tmp`。

### B. VNC 图形桌面

用浏览器 VNC 进桌面后，用桌面自带的上传（若有），或先传到你自己的网盘再在机器上下。

### C. 对象存储 COS（最稳，国内快）

1. Mac 上传 `smelt-signal-linux` 到你的 COS 桶（设**临时**公有读或签名 URL）
2. 网页终端：

```bash
# 换成你的 COS 链接（国内下载通常很快）
curl -fL --connect-timeout 10 --max-time 60 \
  -o /tmp/smelt-signal \
  "https://你的桶.cos.ap-xxx.myqcloud.com/smelt-signal-linux"
chmod +x /tmp/smelt-signal
```

### D. 实在没有上传

把文件发到微信/自己邮箱，在 VNC 桌面浏览器里下载到 VPS——只要**不依赖 VPS 访问 GitHub**即可。

上传后确认：

```bash
ls -lh /tmp/smelt-signal ~/smelt-signal-linux 2>/dev/null
# 记清楚真实路径，下面用 BIN= 指过去
```

若文件在家目录：

```bash
cp ~/smelt-signal-linux /tmp/smelt-signal
chmod +x /tmp/smelt-signal
```

---

## 3. 网页终端：只装进程（先验证，不需要域名）

下面整段复制执行（**不访问 GitHub**）：

```bash
set -e
BIN=/tmp/smelt-signal   # 若路径不同请改这里

echo "[$(date +%H:%M:%S)] 1 安装二进制"
sudo install -m 755 "$BIN" /usr/local/bin/smelt-signal
file /usr/local/bin/smelt-signal

echo "[$(date +%H:%M:%S)] 2 写环境变量"
sudo mkdir -p /etc/smelt
sudo tee /etc/smelt/smelt-signal.env >/dev/null <<'EOF'
SMELT_SIGNAL_BIND=127.0.0.1:7878
SMELT_ROOM_TTL_SECS=3600
RUST_LOG=info
EOF

echo "[$(date +%H:%M:%S)] 3 写 systemd"
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

echo "[$(date +%H:%M:%S)] 4 启动"
sudo systemctl daemon-reload
sudo systemctl enable --now smelt-signal
sleep 1
systemctl is-active smelt-signal
curl -sS --connect-timeout 3 --max-time 5 http://127.0.0.1:7878/health
echo
echo "[$(date +%H:%M:%S)] 完成。应看到 {\"ok\":true,\"rooms\":0}"
```

若某一行长时间无输出：看最后打印的 `[时:分:秒] N …` 是第几步。

失败时：

```bash
sudo journalctl -u smelt-signal -n 40 --no-pager
```

---

## 4. 有域名再上 HTTPS（nginx + certbot，走 apt，不拉国外 Caddy）

安全组放行 **80、443**；域名 A 记录指到该机。

```bash
set -e
DOMAIN=signal.你的域名.com   # 改成真实域名

echo "[$(date +%H:%M:%S)] apt 安装 nginx certbot（可能 1～3 分钟，会有输出）"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -y
sudo apt-get install -y nginx certbot python3-certbot-nginx

echo "[$(date +%H:%M:%S)] 写 nginx 反代"
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
sudo nginx -t
sudo systemctl enable --now nginx
sudo systemctl reload nginx

echo "[$(date +%H:%M:%S)] certbot 申请证书"
sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos \
  --register-unsafely-without-email --redirect

echo "[$(date +%H:%M:%S)] 公网探活"
curl -sS --connect-timeout 10 --max-time 20 "https://${DOMAIN}/health"
echo
```

信令地址：`wss://你的域名/ws`

---

## 常见卡点

| 卡住的感觉 | 实际原因 | 怎么办 |
|------------|----------|--------|
| `curl github.com` 一直转 | 国内机访问 GitHub 差 | **不要在 VPS 下二进制**，用 Mac 下 + 上传 |
| `apt-get update` 慢 | 源慢 | 换腾讯云镜像源后重试；应有滚动输出 |
| 无任何提示 | 命令没 `echo` 进度 | 用上面带 `[时:分:秒]` 的脚本 |
| certbot 失败 | DNS/安全组 | 先 `curl 127.0.0.1:7878/health` 证明进程 OK |

---

## 和 SSH 脚本的关系

| 方式 | 适用 |
|------|------|
| 本文（网页终端） | 无 SSH、控制台登录 |
| `push-from-mac.sh` + `install.sh` | 本机已配置 `ssh user@ip` |

两种最终效果一样：本机 `7878` + 可选 `https://域名`。
