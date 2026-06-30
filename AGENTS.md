# Agent 工作规范

1. **只有用户强调过的内容才记录入本文档；未经用户同意，不得自行添加其他内容。**

2. **上下文丢失时必须向用户确认，禁止猜测执行。**
   当对用户需求、任务范围或历史上下文产生任何不确定时，必须暂停执行并向用户询问确认，而不是自行推断后继续操作。

3. **`litellm/` 作为参考目录不得直接修改或移除。** 如需借鉴其中代码，先复制到 `tmp/` 中操作，完成后删除 `tmp/`。

4. **任务处理与 PR 全生命周期流程（必须严格遵循）：**

   **a. 接收新任务 → 判断工作环境**
   - 主仓 `~/alysechen/github/codex-app-transfer/` 永远 checkout `main` 不参与开发；所有 feature 任务一律在 sibling worktree（`codex-app-transfer-worktrees/<branch>/`）工作。
   - 若已在某个 worktree 中且任务与之相关，继续在该 worktree 上工作。
   - 若已在某个 worktree 中但新任务与之无关，为新任务创建新的 worktree。此时需先检查旧 worktree 的状态（分支、未提交变更、关联 PR 状态等），并在新任务结束时向用户汇报旧 worktree 的详情，方便用户对旧 worktree 做决策。

   **b. 任务完成 → 提交 + 创建 PR + Review**
   - 完成开发后 push 到远端分支，创建 PR 并进行 review。后台监测 PR 状态（CI checks、review threads、merge state），出现失败或阻塞时主动处理。
   - **微改动不立即开 PR**：1 行 yaml / typo / dep bump / README link 等微改动**不**立即开 PR，攒到下一任务搭车，或累 ≥ 5 条 / ≥ 7 天起 `chore: misc micro-fixes` PR（每个 PR CI 跑 Rust Tauri check 5–15 min，减少资源浪费）。
   - **README 同 PR 更新**：影响用户可见行为的 PR 前必须同步改 `README.md` / `README.en.md`，跟代码同 PR 提交，不要事后单独发 README PR。
   - **上游借鉴致谢**：借鉴上游开源项目（litellm / AiMaMi 等）代码 / prompt / 架构时，README 中英致谢段同 PR 加注明，不要漏不要事后补。
   - **PR `BLOCKED` 时必查 unresolved review threads**：CI 全绿但 `mergeStateStatus=BLOCKED` 多半是 `chatgpt-codex-connector` / `devin-ai-integration` 留了 unresolved review thread + 仓库开启 `required_conversation_resolution=true`。用 GraphQL 查 `reviewThreads`，反馈已在后续 commit 处理 → `addPullRequestReviewThreadReply` + `resolveReviewThread` mutation 关闭。**禁止用 `--admin` 跳保护。**
   - **报"已完成"前必 verify**：任何"已 X / X 完成"断言前先跑 `pgrep` / `stat` / `gh api` 等独立 verify，报告时引用 verify 输出，不凭 exit 0 报告。

   **c. PR 全绿 → 等待用户指令**
   - CI 和 review 全部通过后，向用户汇报状态，由用户判断是 merge 还是有后续任务。
   - **禁止自动 merge**，必须等用户显式声明。

   **d. 后续任务 → 同 worktree / 同 PR 追加 commit**
   - **本对话内**有后续任务时，继续在同一 worktree 和同一 PR 上追加 commit 处理，不开新 PR。
   - **跨对话 PR 不在搭车范围**：跨 session 的 open PR 视为独立任务，默认开新 PR（避免跨 session 工作摞在一个 PR 上，除非用户显式指示搭车）。

   **e. 用户显式声明 merge → rebase + 完整清理**
   - **e0. （stacked PR only）解耦 child PR base**：merge 前若存在以本 PR head branch 为 base 的 open child PR，必须先 `gh pr edit <child> --base main`。否则 `gh pr merge --squash --delete-branch` 删 head branch 时 GitHub **会自动关闭** child PR（不是改 base，是 CLOSED + base ref 不存在），补救需 4 步 API mutation 重建 ref → reopen → 改 base → 删 ref。
   - **e1. Rebase**：先对目标分支执行 `rebase`；若无冲突或冲突少且简单，AI 自行解决后继续；若冲突较多或涉及复杂逻辑 / 重要决策，必须向用户提供解决方案并获得确认后再执行，**禁止自行决定修改方向**。**特殊情况**：child PR base 已被 squash 进 main 时 rebase 大概率假冲突（squash merge 不是 patch-identical 原始 commits）→ `git rebase --abort` → `git reset --hard origin/main` → cherry-pick 该 PR 独有 commits，**不要硬继续 rebase**。
   - **e2. Merge + 远端 silent delete verify**：`gh pr merge <PR#> --squash --delete-branch` 后必须验证远端 ref 真删 —— **不能直接看 `git ls-remote` 的 exit code**（连接成功即 0，跟 ref 存不存在无关），改用 `git ls-remote --heads --exit-code origin <branch>`（ref 不存在时 exit 2）**或** `[ "$(git ls-remote --heads origin <branch> | wc -l)" -eq 0 ]`（stdout 0 行 = ref 不存在）；残留时手动 `git push origin --delete <branch>`（worktree 锁本地分支时 gh 也 skip remote delete，silent failure）。
   - **e3. 本地清理**：`git worktree remove <path>` → `git branch -D <branch>` → `git worktree prune` → 清理 `src-tauri/target/release/bundle/macos/` 等 build 残留。
   - **e4. 回归 main + 同步**：`git checkout main` → `git pull --ff-only origin main`。
   - **e6. 关联 issue + Linear followup 更新**：`gh issue view <ISSUE#> --json state,closedByPullRequestsReferences` 验证是否被 PR `Closes #N` 自动关，否则手动 `gh issue close <N>`。**Linear followup（workspace Mochance / team Mochance / label Improvement）跟 GitHub issue 是两套独立系统**：本次 PR 实施掉的 Linear issue（MOC-N）用 `mcp__linear__save_issue` 改 `state=Done`，并在 issue body 末尾追加 resolved PR 链接。（历史 `docs/followup-tracker.md` 制度 2026-05-24 起停用，新工作流不再写本地 .md。`docs/` 整目录已 gitignored。）

5. **发版流程**

   **a. 预发布微 PR（版本号 bump + release notes + CHANGELOG）**
   - 用户说「发版」/「trigger workflow」/「发 vX.Y.Z」时，新版本号默认 patch +1（如 2.4.1 → 2.4.2）；用户没显式指明 bump 档位时**不**按 feat 数量自行推 minor/major，只有用户明说（如「发 2.5.0」「升 minor」）才用更大档位。
   - 在 sibling worktree 开 `chore/release-vX.Y.Z` 分支，三件事同 PR：
     - **A. 全盘扫描更新版本号**：`rg -l "\b<OLD>\b" --type-not lock` 确认没漏。必改 `src-tauri/Cargo.toml` `[package].version` + `src-tauri/tauri.conf.json` top-level `"version"`（`Cargo.lock` 由 `cargo check -p codex-app-transfer` 自动 sync）；可能要改 `README.md` / `README.en.md` 版本行。改完跑 `cargo check -p codex-app-transfer` 让 Cargo.lock 同步。
     - **B. 撰写 release notes**：创建 `release-notes/vX.Y.Z.md`（MOC-66 CI 门禁强制，`scripts/check_release_notes_present.py` 校验，缺文件 release.yml 直接 fail）。格式参照 `release-notes/v2.1.7.md` 标杆：`# Codex App Transfer vX.Y.Z` 标题 + `## 中文` / `## English` 两段，每段单 `###` 主题 + bullets + 验证段；2500-3200 字符；禁粗体 / 删除线 / 中文引号（「」『』）；允许 inline code。
     - **C. CHANGELOG 补条目**：`CHANGELOG.md` 顶部（`## v<上一个版本>` 之前）插入 `## vX.Y.Z` 段，中英双语概述本版改动（参照既有格式）。
   - 微 PR 不立即开 —— 可攒到下一个 feature PR 搭车（同 merge 收尾 d 规则），或累 ≥ 5 条微改动起 `chore: misc micro-fixes` PR。但**发版触发时必须独立开 PR**（release notes 文件 + 版本号必须在 main 上才能 dispatch release.yml）。
   - PR title：`chore(release): prepare vX.Y.Z`。merge 走标准 merge 收尾流程（e0-e4 + e6）。

   **b. dispatch release.yml**
   - 预发布 PR merge 进 main 后，触发 `release.yml` workflow dispatch（`gh workflow run release.yml -f version=X.Y.Z` 或 GitHub Actions UI 手动 dispatch）。dispatch 前确认 main 已含版本号 bump + release-notes 文件。
   - release.yml 跑 `check_release_notes_present.py` 门禁 → 打包 macOS .app/.dmg → 创建 draft release。

   **c. draft release → 注入 notes → 等用户 publish**
   - release.yml 创建的 draft release 用 `release-notes/vX.Y.Z.md` 全文作为 body（不走 GitHub 默认「What's Changed」footer）。
   - 等 release.yml 全绿、draft release 创建成功后，向用户汇报 release URL + 打包产物，**等用户显式声明 publish** 才点 publish（禁止自动 publish）。
   - publish 后打 tag `vX.Y.Z`（release.yml 自动打），验证 `gh release view vX.Y.Z` 状态为 published。
