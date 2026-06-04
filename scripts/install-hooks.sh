#!/usr/bin/env bash
# MOC-138 Tier 3: 启用本仓 pre-push 门禁(一次性,每个 clone 跑一次即可)。
#
# 用相对路径 .githooks —— git 把相对 core.hooksPath 按「当前工作树根」解析,
# 天然适配多 worktree 流(每个 worktree 用自己 checkout 的 .githooks/pre-push)。
# 注: core.hooksPath 是 per-clone 本地配置,不进 git,故每个 clone 要装一次。
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

git config core.hooksPath .githooks
chmod +x .githooks/* scripts/*.sh 2>/dev/null || true

echo "✅ core.hooksPath → .githooks,pre-push 门禁已启用"
echo "   验证: git config core.hooksPath   (应输出 .githooks)"
echo "   卸载: git config --unset core.hooksPath"
