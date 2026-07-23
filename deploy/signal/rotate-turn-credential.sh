#!/usr/bin/env bash
# 手动轮换 coturn 的 REST API 共享密钥（static-auth-secret）。
#
# 换成 REST API 临时凭证模式（use-auth-secret）之后，这个脚本已经不是"堵漏"
# 用的了——凭证本身短时效自动过期，泄露了也用不了多久，两边（turnserver.conf
# 和 smelt-signal.env）也不会像旧版 lt-cred-mech 那样因为手动改错而悄悄分叉。
# 轮换这份共享密钥纯粹是防御纵深（怀疑密钥被看到过、定期例行轮换），不是必须
# 操作，不跑也不影响正常使用。
#
# 用法（root/sudo，机器上已经跑过 install-coturn.sh）：
#   sudo bash rotate-turn-credential.sh
#   # 或指定新密钥：
#   sudo TURN_SECRET=新密钥 bash rotate-turn-credential.sh
set -euo pipefail

log() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "✗ $*" >&2; exit 1; }

[[ "$(id -u)" -eq 0 ]] || die "请用 sudo / root 运行"

CONF=/etc/turnserver.conf
ENV_FILE=/etc/smelt/smelt-signal.env

[[ -f "$CONF" ]] || die "找不到 $CONF，先跑 install-coturn.sh"
[[ -f "$ENV_FILE" ]] || die "找不到 $ENV_FILE"

grep -q '^use-auth-secret' "$CONF" || die \
  "$CONF 里没有 use-auth-secret——还在用旧版 lt-cred-mech？重新跑一遍 install-coturn.sh 迁移到 REST API 模式再用这个脚本"

TURN_SECRET="${TURN_SECRET:-$(openssl rand -base64 32 | tr -d '/+=' | head -c 32)}"

log ">>> 1/3 更新 $CONF"
sed -i "s/^static-auth-secret=.*/static-auth-secret=${TURN_SECRET}/" "$CONF"

log ">>> 2/3 更新 $ENV_FILE 的 SMELT_TURN_SECRET"
grep -v '^SMELT_TURN_SECRET=' "$ENV_FILE" >"${ENV_FILE}.tmp" || true
mv "${ENV_FILE}.tmp" "$ENV_FILE"
printf 'SMELT_TURN_SECRET=%s\n' "$TURN_SECRET" >>"$ENV_FILE"

log ">>> 3/3 重启 coturn + smelt-signal"
systemctl restart coturn 2>/dev/null || systemctl restart turnserver 2>/dev/null || die "找不到 coturn/turnserver systemd unit"
if systemctl is-enabled smelt-signal &>/dev/null || systemctl is-active smelt-signal &>/dev/null; then
  systemctl restart smelt-signal
else
  log "未检测到 smelt-signal.service，请自行重启并确认 EnvironmentFile 生效"
fi

echo
echo "======== 新密钥（仅此一次打印，请存密码管理器） ========"
echo "TURN secret: $TURN_SECRET"
echo "========================================================"
echo
echo "旧密钥算出来的临时凭证立即失效；正在用旧凭证的中继连接下次重连/ICE"
echo "restart 会失败（属预期，重新走一遍 hello 就能拿到新凭证）。"
