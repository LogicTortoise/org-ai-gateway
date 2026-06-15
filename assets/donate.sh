#!/bin/sh
# org-ai-gateway 远程捐号脚本（方案 A）。
#
# 在你自己的机器上运行——它读取本机已登录的 Codex / Claude / Cursor 凭据，
# 通过 HTTPS 提交到远程网关。等价于 UI 里的"本机导入"，只是读取发生在
# 捐号人本机（而不是网关服务器），因此能用于远程共享部署。
#
# 用法：
#   curl -fsSL https://你的网关/donate.sh | sh                  # 捐出全部已登录的账号
#   curl -fsSL https://你的网关/donate.sh | USER_ID=koltyu sh    # 指定身份
#   curl -fsSL https://你的网关/donate.sh | sh -s -- codex      # 只捐 codex（claude / cursor 同理）
#
# 环境变量：
#   GATEWAY      网关地址（默认：脚本被下载时由网关注入；可覆盖）
#   USER_ID      捐号归属的用户标识（默认：当前系统用户名）
#   EDGE_SECRET  若网关启用了可信边缘密钥(GATEWAY_EDGE_SECRET)，在此提供以走可信头
#   LABEL        账号显示名（默认：USER_ID）
set -eu

GATEWAY="${GATEWAY:-__GATEWAY_BASE_URL__}"
GATEWAY="${GATEWAY%/}"
USER_ID="${USER_ID:-$(id -un)}"
LABEL="${LABEL:-$USER_ID}"
EDGE_SECRET="${EDGE_SECRET:-}"

# 哪些 provider 要捐：命令行参数优先，否则全部自动探测。
PROVIDERS="${*:-codex claude cursor}"

if [ -z "$GATEWAY" ] || [ "$GATEWAY" = "__GATEWAY_BASE_URL__" ]; then
    echo "错误：未知网关地址。请设置 GATEWAY=https://你的网关 后重试。" >&2
    exit 1
fi

# ---- JSON 字符串转义（python3 优先，jq 次之，awk 兜底）----------------------
json_escape() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -c 'import json,sys;sys.stdout.write(json.dumps(sys.stdin.read()))'
    elif command -v jq >/dev/null 2>&1; then
        jq -Rs .
    else
        awk 'BEGIN{ORS="";print "\""}
             {gsub(/\\/,"\\\\");gsub(/"/,"\\\"");gsub(/\t/,"\\t");gsub(/\r/,"\\r")}
             NR>1{print "\\n"} {printf "%s",$0}
             END{print "\""}'
    fi
}

# ---- 提交一个账号到网关 ------------------------------------------------------
# 参数：provider 接口路径 字段名 凭据内容
post() {
    _provider="$1"; _path="$2"; _field="$3"; _cred="$4"
    _label_json=$(printf '%s' "$LABEL" | json_escape)
    _cred_json=$(printf '%s' "$_cred" | json_escape)
    _body=$(printf '{"account_label":%s,"share_enabled":true,"daily_token_limit":null,"%s":%s}' \
        "$_label_json" "$_field" "$_cred_json")

    set -- -sS -X POST "$GATEWAY$_path" -H "Content-Type: application/json"
    if [ -n "$EDGE_SECRET" ]; then
        set -- "$@" -H "X-Gateway-Auth: $EDGE_SECRET" -H "X-User-Id: $USER_ID"
    else
        set -- "$@" -H "Authorization: Bearer user:$USER_ID"
    fi

    _resp=$(printf '%s' "$_body" | curl "$@" --data-binary @- 2>&1) || {
        echo "  ✗ $_provider：请求失败：$_resp" >&2
        return 1
    }
    case "$_resp" in
        *'"account_id"'*) echo "  ✓ $_provider：已捐出（owner=$USER_ID）" ;;
        *'"error"'*)      echo "  ✗ $_provider：$_resp" >&2; return 1 ;;
        *)                echo "  ? $_provider：响应异常：$_resp" >&2; return 1 ;;
    esac
}

donate_codex() {
    _f="$HOME/.codex/auth.json"
    if [ ! -f "$_f" ]; then
        echo "  - codex：未找到 $_f（请先在本机 codex login），跳过"
        return 0
    fi
    post codex "/v1/provider/connect/codex/auth-json" "auth_json" "$(cat "$_f")" || true
}

donate_claude() {
    _cred=""
    if [ "$(uname)" = "Darwin" ]; then
        # macOS：Claude Code 把凭据放在登录 Keychain，不是文件。
        _cred=$(security find-generic-password -a "$(id -un)" -w -s "Claude Code-credentials" 2>/dev/null || true)
    fi
    if [ -z "$_cred" ]; then
        for _p in \
            "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json" \
            "$HOME/.claude/.credentials.json" \
            "$HOME/.config/claude/.credentials.json" \
            "$HOME/.claude/credentials.json" \
            "$HOME/.anthropic/credentials.json"; do
            if [ -f "$_p" ]; then _cred=$(cat "$_p"); break; fi
        done
    fi
    if [ -z "$_cred" ]; then
        echo "  - claude：未找到登录凭据（请先在本机登录 Claude Code），跳过"
        return 0
    fi
    post claude "/v1/provider/connect/claude/auth-json" "auth_json" "$_cred" || true
}

cursor_db_path() {
    case "$(uname)" in
        Darwin) printf '%s' "$HOME/Library/Application Support/Cursor/User/globalStorage/state.vscdb" ;;
        *)      printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/Cursor/User/globalStorage/state.vscdb" ;;
    esac
}

donate_cursor() {
    if ! command -v sqlite3 >/dev/null 2>&1; then
        echo "  - cursor：需要 sqlite3 才能读取本机登录态，跳过"
        return 0
    fi
    _db=$(cursor_db_path)
    if [ ! -f "$_db" ]; then
        echo "  - cursor：未找到 $_db（请先在本机登录 Cursor），跳过"
        return 0
    fi
    _token=$(sqlite3 -cmd ".timeout 3000" "$_db" \
        "SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken';" 2>/dev/null || true)
    if [ -z "$_token" ]; then
        echo "  - cursor：未登录或未找到 accessToken，跳过"
        return 0
    fi
    post cursor "/v1/provider/connect/cursor" "session_token" "$_token" || true
}

echo "→ 网关：$GATEWAY"
echo "→ 捐号归属：$USER_ID"
echo "→ 捐出：$PROVIDERS"
echo

for _p in $PROVIDERS; do
    case "$_p" in
        codex)  donate_codex ;;
        claude) donate_claude ;;
        cursor) donate_cursor ;;
        *)      echo "  - 未知 provider：$_p（支持 codex / claude / cursor），跳过" ;;
    esac
done

echo
echo "完成。可在网关 UI 的 Upstreams 标签页查看已捐出的账号。"
