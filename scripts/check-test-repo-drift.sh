#!/usr/bin/env bash
# MOC-138 Tier 4: 检测 codex-app-transfer_test 独立 clone 是否跟远端 main 漂移。
#
# 背景: codex-app-transfer_test 是独立 clone(非 worktree),有自己的 .git/,
# 不随主仓 worktree 列表同步。在里面 apply_patch / 写代码会写到独立 git,容易
# 误以为「没动主仓」其实写错地方了 —— 这个脚本主动预警漂移。
#
# 漂移 = 以下任一:工作区未提交改动(dirty)/ 有本地未推提交(ahead)/ 落后远端
# main(behind)。dirty 与 ahead 正是「误在独立 clone 里写了东西」的信号。
# 注:dirty / HEAD 是纯本地检测,offline 也能查;故先查本地,fetch 失败(offline)
# 只降级跳过 behind/ahead 比对,绝不因此漏报本地 dirty。
#
# 用法: scripts/check-test-repo-drift.sh   (设 TEST_REPO 环境变量覆盖路径)
# 退出码: 0=干净(同步,或 offline 但工作区干净) / 跳过(repo 不存在), 1=漂移
set -euo pipefail

# 默认取主仓同级的 codex-app-transfer_test(可移植,不写死个人绝对路径)。
# 用 --git-common-dir 兼容 worktree:始终解析到主仓而非当前 worktree 目录。
_common="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
_default_test_repo="${_common:+$(cd "$(dirname "$_common")/.." 2>/dev/null && pwd)/codex-app-transfer_test}"
TEST_REPO="${TEST_REPO:-${_default_test_repo:-../codex-app-transfer_test}}"

if [[ ! -d "$TEST_REPO/.git" ]]; then
  echo "⚠️ test repo 不在 $TEST_REPO,跳过(设 TEST_REPO 环境变量覆盖)"
  exit 0
fi

# —— 纯本地检测(不依赖网络,offline 也查)——
TEST_HEAD=$(git -C "$TEST_REPO" rev-parse HEAD)
DIRTY=$(git -C "$TEST_REPO" status --porcelain)

# —— 远端比对(需 fetch origin/main;offline 则降级,只跳过 behind/ahead)——
BEHIND=0
AHEAD=0
REMOTE_MAIN="(offline,未比对)"
NET_OK=0
if git -C "$TEST_REPO" fetch origin main --quiet 2>/dev/null; then
  NET_OK=1
  REMOTE_MAIN=$(git -C "$TEST_REPO" rev-parse origin/main)
  BEHIND=$(git -C "$TEST_REPO" rev-list --count HEAD..origin/main)
  AHEAD=$(git -C "$TEST_REPO" rev-list --count origin/main..HEAD)
fi

if [[ -z "$DIRTY" && "$BEHIND" == "0" && "$AHEAD" == "0" ]]; then
  if [[ "$NET_OK" == "1" ]]; then
    echo "✅ codex-app-transfer_test 跟远端 main 同步且工作区干净 (${TEST_HEAD:0:12})"
  else
    echo "⚠️ 无法 fetch origin/main(offline?),未比对 behind/ahead;工作区干净 (${TEST_HEAD:0:12})"
  fi
  exit 0
fi

echo "⚠️ codex-app-transfer_test 与远端 main 漂移:"
if [[ -n "$DIRTY" ]]; then
  echo "   • 工作区有 $(printf '%s\n' "$DIRTY" | wc -l | tr -d ' ') 处未提交改动 —— 可能误在独立 clone 里写了代码"
fi
if [[ "$AHEAD" != "0" ]]; then
  echo "   • 领先 $AHEAD commits(独立 clone 里有未推的本地提交?)"
fi
if [[ "$BEHIND" != "0" ]]; then
  echo "   • 落后 $BEHIND commits —— 同步: git -C \"$TEST_REPO\" pull --ff-only origin main"
fi
if [[ "$NET_OK" == "1" ]]; then
  echo "   test HEAD: ${TEST_HEAD:0:12}   remote main: ${REMOTE_MAIN:0:12}"
else
  echo "   • (offline:未比对远端 behind/ahead,仅报本地 dirty)"
  echo "   test HEAD: ${TEST_HEAD:0:12}"
fi
exit 1
