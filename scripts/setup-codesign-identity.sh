#!/usr/bin/env bash
# 生成一个稳定的自签名代码签名身份，供 package-mac.sh 使用。
#
# 解决的问题：ad-hoc 签名（--sign -）的身份直接从二进制内容算哈希，每次重新编译
# /升级二进制内容一变，身份跟着变，macOS TCC（完全磁盘访问权限等）就认不出是同一个
# app，导致权限反复失效要重新弹窗授权。自签名证书的身份绑定在证书公钥上，只要一直
# 用同一张证书签名，重新编译/升级都不会改变身份，TCC 授权就能持久。
#
# 不需要付费 Apple Developer 账号；代价是仍然过不了公证，Gatekeeper 对外部用户还是
# 会提示「来自身份不明的开发者」——这个只解决权限持久化，不解决分发信任问题。
#
# 用法：
#   ./scripts/setup-codesign-identity.sh
#
# 产出：
#   - 登录钥匙串里一个名为 "${CERT_CN}" 的代码签名身份（codesign 直接可用）
#   - ~/.smelt/codesign/smelt-codesign.p12（备份，导入密码见同目录 .password 文件）
#     —— 需要在 CI 里签名时，把这个 p12 base64 后存进 GitHub secrets。
set -euo pipefail

CERT_CN="Smelt Local Signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
OUT_DIR="$HOME/.smelt/codesign"
P12_PATH="$OUT_DIR/smelt-codesign.p12"
PASS_PATH="$OUT_DIR/smelt-codesign.p12.password"

if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "\"${CERT_CN}\""; then
  echo "✓ 登录钥匙串里已存在签名身份「${CERT_CN}」，跳过生成"
  if [[ -f "${P12_PATH}" ]]; then
    echo "✓ p12 备份已存在：${P12_PATH}"
  else
    echo "⚠ 钥匙串里有身份，但找不到 p12 备份（${P12_PATH}）。"
    echo "  若要接入 CI 签名，需要重新生成：先在「钥匙串访问」里删掉「${CERT_CN}」这个身份，再重跑本脚本。"
  fi
  exit 0
fi

echo "▶ 生成自签名代码签名证书「${CERT_CN}」…"
mkdir -p "$OUT_DIR"
chmod 700 "$OUT_DIR"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

openssl req -x509 -newkey rsa:2048 -keyout "$WORKDIR/key.pem" -out "$WORKDIR/cert.pem" \
  -days 3650 -nodes -subj "/CN=${CERT_CN}" \
  -addext "extendedKeyUsage=codeSigning" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" >/dev/null 2>&1

# p12 导出密码随机生成，本地留一份供后续拷去 GitHub secrets；这张证书只用来给
# TCC 提供稳定身份锚点，不是什么高价值密钥，本地明文存密码可接受。
# -legacy：OpenSSL 3.x 默认用 AES+SHA256 加密 pkcs12，macOS `security import`
# 认的是老式 3DES/RC2，不加这个会报「MAC verification failed（像密码错但其实
# 是算法不兼容）」。
P12_PASSWORD="$(openssl rand -base64 24)"
openssl pkcs12 -export -legacy -out "${P12_PATH}" \
  -inkey "$WORKDIR/key.pem" -in "$WORKDIR/cert.pem" \
  -passout "pass:$P12_PASSWORD" -name "${CERT_CN}"
printf '%s' "$P12_PASSWORD" > "$PASS_PATH"
chmod 600 "${P12_PATH}" "$PASS_PATH"

echo "▶ 导入登录钥匙串…"
security import "${P12_PATH}" -k "$KEYCHAIN" -P "$P12_PASSWORD" \
  -T /usr/bin/codesign -T /usr/bin/security

echo ""
echo "✅ 完成"
security find-identity -v -p codesigning "$KEYCHAIN" | grep "${CERT_CN}" || \
  echo "  （find-identity 未列出属正常——self-signed 证书没有系统信任链，不影响 codesign 直接使用）"
echo ""
echo "p12 备份：  ${P12_PATH}"
echo "导入密码：  $PASS_PATH"
echo ""
echo "下一步：scripts/package-mac.sh 会自动探测「${CERT_CN}」这个身份并用它签名，无需额外配置。"
echo "要接入 CI（GitHub Actions）签名，把这两个存进 repo secrets："
echo "  gh secret set CODESIGN_P12_BASE64 < <(base64 -i \"${P12_PATH}\")"
echo "  gh secret set CODESIGN_P12_PASSWORD < \"$PASS_PATH\""
