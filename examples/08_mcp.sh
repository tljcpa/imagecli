#!/usr/bin/env bash
# 用途: 把 imagecli 当 MCP server 用(stdio 上的 JSON-RPC 2.0), 供 Claude Code / Cursor 等 agent 调用。
#   本脚本演示手动喂两条请求: initialize 与 tools/list, 看 server 回什么。
# 需要的 key 环境变量: 无(仅 initialize/tools/list 只读握手); 真正 generate_* 工具才需对应 provider key。
# 提醒: 绝不要把真实 key 硬编码进脚本, 用环境变量注入(MCP 生成工具的 key 走 server 进程环境变量)。
set -euo pipefail

# MCP server 从 stdin 逐行读 JSON-RPC 请求, 逐行回 JSON 响应。
# 这里用 printf 拼两行请求(每行一个完整 JSON), 管道喂给 `imagecli mcp`:
#   第 1 行: initialize (握手, 声明 protocolVersion; server 会回显)
#   第 2 行: tools/list (列出暴露的 6 个工具)
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | imagecli mcp

# 调用某个工具用 tools/call, 例如列出 provider(只读, 不消耗额度):
#   printf '%s\n' \
#     '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_providers","arguments":{}}}' \
#     | imagecli mcp
#
# 文生图工具(会真实消耗额度, 需 server 进程里已 export 对应 key):
#   printf '%s\n' \
#     '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"generate_image","arguments":{"provider":"agnes","prompt":"a red fox in snow"}}}' \
#     | imagecli mcp
