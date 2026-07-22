#!/usr/bin/env bash
# 手动轮换 coturn 的静态 TURN 密码。
#
# lt-cred-mech 的长期凭证没有过期时间：hello_ok 把它发给每一个连上信令的客户端，
# 泄露了就能被当公开中继一直用到你换密码为止。这个脚本不解决"静态凭证"本身的
# 问题（真要解决得换 coturn REST API 临时凭证），只是把风险敞口从"无限期"收窄到
# "两次轮换之间"——建议定期手动跑，或自己包一个 cron。
#
# 用法（root/sudo，机器上已经跑过 install-coturn.sh）：
#   sudo bash rotate-turn-credential.sh
#   # 或指定新密码：
#   sudo TURN_PASS=新密码 bash rotate-turn-credential.sh
set -euo pipefail

log() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "✗ $*" >&2; exit 1; }

[[ "$(id -u)" -eq 0 ]] || die "请用 sudo / root 运行"

CONF=/etc/turnserver.conf
ENV_FILE=/etc/smelt/smelt-signal.env

[[ -f "$CONF" ]] || die "找不到 $CONF，先跑 install-coturn.sh"
[[ -f "$ENV_FILE" ]] || die "找不到 $ENV_FILE"

TURN_USER="$(grep -oP '^user=\K[^:]+' "$CONF" || true)"
[[ -n "$TURN_USER" ]] || die "读不到 $CONF 里的 user=，配置格式和 install-coturn.sh 生成的不一致？"

OLD_ICE_LINE="$(grep '^SMELT_ICE_SERVERS=' "$ENV_FILE" || true)"
[[ -n "$OLD_ICE_LINE" ]] || die "读不到 $ENV_FILE 里的 SMELT_ICE_SERVERS=，改用 install-coturn.sh 重装"

# 从现有 SMELT_ICE_SERVERS 里抠出 turn: 的 host（install-coturn.sh 写入时用的是
# DOMAIN 或 PUBLIC_IP），保证轮换后 ICE host 不变，只换密码。
ICE_HOST="$(echo "$OLD_ICE_LINE" | grep -oP '"turn:\K[^:"?]+' | head -1)"
[[ -n "$ICE_HOST" ]] || die "解析不出当前 ICE host，SMELT_ICE_SERVERS 格式被手改过？"

TURN_PASS="${TURN_PASS:-$(openssl rand -base64 24 | tr -d '/+=' | head -c 24)}"

log ">>> 1/3 更新 $CONF"
sed -i "s/^user=.*/user=${TURN_USER}:${TURN_PASS}/" "$CONF"

log ">>> 2/3 重建 $ENV_FILE 的 SMELT_ICE_SERVERS"
ICE_JSON=$(cat <<JSON
[{"urls":"stun:${ICE_HOST}:3478"},{"urls":["turn:${ICE_HOST}:3478?transport=udp","turn:${ICE_HOST}:3478?transport=tcp"],"username":"${TURN_USER}","credential":"${TURN_PASS}"},{"urls":"stun:stun.qq.com:3478"},{"urls":"stun:stun.miwifi.com:3478"},{"urls":"stun:stun.cloudflare.com:3478"},{"urls":"stun:stun.l.google.com:19302"}]
JSON
)
ICE_JSON="$(echo "$ICE_JSON" | tr -d '\n')"
grep -v '^SMELT_ICE_SERVERS=' "$ENV_FILE" >"${ENV_FILE}.tmp" || true
mv "${ENV_FILE}.tmp" "$ENV_FILE"
printf 'SMELT_ICE_SERVERS=%s\n' "$ICE_JSON" >>"$ENV_FILE"

log ">>> 3/3 重启 coturn + smelt-signal"
systemctl restart coturn 2>/dev/null || systemctl restart turnserver 2>/dev/null || die "找不到 coturn/turnserver systemd unit"
if systemctl is-enabled smelt-signal &>/dev/null || systemctl is-active smelt-signal &>/dev/null; then
  systemctl restart smelt-signal
else
  log "未检测到 smelt-signal.service，请自行重启并确认 EnvironmentFile 生效"
fi

echo
echo "======== 新密码（仅此一次打印，请存密码管理器） ========"
echo "TURN user:     $TURN_USER"
echo "TURN password: $TURN_PASS"
echo "========================================================"
echo
echo "旧密码已失效；用旧凭证连着的中继流量下次重连会失败（属预期）。"
