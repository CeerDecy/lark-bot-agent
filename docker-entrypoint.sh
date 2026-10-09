#!/bin/sh
# 根据 AGENT_KIND 把 AGENT_COMMAND 指向镜像里预装的适配器，避免运行时 npx 下载。
# 用户显式设置了 AGENT_COMMAND 时尊重用户的值。
set -eu

if [ -z "${AGENT_COMMAND:-}" ]; then
  case "${AGENT_KIND:-}" in
    codex)  export AGENT_COMMAND="$(command -v codex-acp)" ;;
    claude) export AGENT_COMMAND="$(command -v claude-agent-acp)" ;;
    *) ;;  # 交给 lark-bot-agent 自己报错
  esac
fi

exec "$@"
