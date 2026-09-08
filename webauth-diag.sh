#!/bin/bash
# dsh-remote web-auth 链路隔离测试（在 clawith-test 本机以 root 运行）
# 逐环验证：设备令牌 → web-auth 桥 → token 换上游会话 → 会话直连
# 每步输出状态码与关键头，哪步断哪步就是根因层。

GATEWAY="http://127.0.0.1:3090"
WEB="http://127.0.0.1:3080"

echo "=== 0. 前置健康检查 ==="
echo -n "3080 web: "; curl -s -o /dev/null -w "%{http_code}\n" --max-time 5 "$WEB/"
echo -n "3090 网关: "; curl -s -o /dev/null -w "%{http_code}\n" --max-time 5 "$GATEWAY/"

echo ""
echo "=== 0.5 status API 日志尾（看 connection 捕获行/发布行） ==="
curl -s --max-time 5 "$WEB/dsh-remote/api/status" | grep -oP '"log":\[[^\]]*\]' | head -c 3000
echo ""

echo ""
echo "=== 1. 造测试设备令牌 ==="
PAIR_JSON=$(curl -s --max-time 5 -X POST "$WEB/dsh-remote/api/pairing")
CODE=$(echo "$PAIR_JSON" | grep -oP '"code":"\K[^"]+')
if [ -z "$CODE" ]; then
  echo "FAIL: 拿不到配对码。pairing API 原始返回："
  echo "$PAIR_JSON" | head -c 500
  exit 1
fi
echo "code=${CODE:0:12}..."

PAIR_RESP=$(curl -s --max-time 5 -X POST "$GATEWAY/__remote/pair" -H "Content-Type: application/json" -d "{\"code\":\"$CODE\"}")
TOKEN=$(echo "$PAIR_RESP" | grep -oP '"token":"\K[^"]+')
if [ -z "$TOKEN" ]; then
  echo "FAIL: 拿不到设备令牌。pair 原始返回："
  echo "$PAIR_RESP" | head -c 500
  exit 1
fi
echo "token_len=${#TOKEN}"

echo ""
echo "=== 2. web-auth 桥（设备凭证 → 302 /?token=<launchToken>?） ==="
STEP2=$(curl -si --max-time 5 "$GATEWAY/__remote/web-auth" -H "x-remote-token: $TOKEN")
echo "$STEP2" | head -8
LOC=$(echo "$STEP2" | grep -i '^location:' | tr -d '\r' | sed 's/^[Ll]ocation: //')
WEBTOK=$(echo "$LOC" | grep -oP 'token=\K[^;&]+')
echo ""
if [ -z "$WEBTOK" ]; then
  echo ">>> 断点判定：Location 是裸「$LOC」（无 token）——"
  echo ">>> v0.2.7 的 connection 注入没生效（桥空转退级）。查上面 status 日志里有没有「已捕获宿主 connection 服务」行。"
  exit 2
fi
echo "webtok=${WEBTOK:0:24}... ✓ 桥发 token 了"

echo ""
echo "=== 3. token 换上游会话（302 + Set-Cookie 预期） ==="
STEP3=$(curl -si --max-time 5 "$GATEWAY/?token=$WEBTOK")
echo "$STEP3" | head -8
SESS=$(echo "$STEP3" | grep -i '^set-cookie:' | head -1 | sed 's/^[Ss]et-[Cc]ookie: //')
if [ -z "$SESS" ]; then
  echo ">>> 断点判定：上游没种会话 cookie（仍 401？）——上游验 token 失败。"
  echo ">>> 候选：token 单次消费 / Host(authority) 绑定校验 / query 被网关代理层改写"
  exit 3
fi
echo "会话 cookie 已种：${SESS:0:60}..."

echo ""
echo "=== 4. 会话直连（200 = 全链路通） ==="
STEP4=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 "$GATEWAY/" -H "Cookie: $SESS")
echo "携带会话 cookie 访问 / → HTTP $STEP4"
if [ "$STEP4" = "200" ]; then
  echo ""
  echo "===== 全链路通 ✅ 服务端无问题，断点在桌面侧导航链 ====="
else
  echo ">>> 会话 cookie 未解锁上游（域/路径/签名问题）"
fi
