# 并行开发规则

这个仓库经常有多个 agent 同时改同一张地图（#112 S4）。共享 main 曾被并行 session 的
`git checkout` / `git reset` / `git clean` 清掉过未提交文件。规则：**main 只做集成，
实现永远在自己的 worktree 里做。**

## 开工

worktree **一律放仓库内 `.worktrees/`**（已 git 忽略），默认 `.worktrees/<ticket>`（例如
`.worktrees/198`）。放仓库内是因为本机沙箱默认只给 workspace 读写权限；仓库外（旧的
`<repo>.worktrees/`）不再作为默认。不要放 `target/`：`cargo clean` 会删掉整个 `target/`，
藏在里面的 worktree 会一起消失；`worktree-add` 会拒绝落在 `target/` 下的根。

```sh
node tools/agent-preflight.mjs --fetch   # 一行看清分支 / ahead-behind / dirty / worktree 数
node tools/worktree-add.mjs <ticket>     # 先校验票存在且未关闭，再打印 worktree 路径
```

`worktree-add` 默认从 `origin/main` 建 `feat/<ticket>`；需要时用 `--base` / `--branch`，
`--print` 只打印路径，`--force` 跳过票校验（hotfix 用）。换根时设 `SVCD_WT_ROOT=<dir>`，
仍然只拒绝落在 `target/` 下的根。

票号必须是**已存在且未关闭**的票：`worktree-add` 会打印 `ticket: #N <标题> [labels]`，标题
与你要做的事不符就停下——#174 那次就是把别的 session 的票号当成了自己的。开工前用
`deck_work_route` 看一眼本工作区 24 小时的在办表，确认票没被别的 session 拿走。

历史遗留的 `target/wt-*` 与 `<repo>.worktrees/` 是旧布局：`node tools/agent-preflight.mjs --all`
会把 `target/` 里的标成 `[under target!]`，`--strict` 会失败。已合并且干净的用
`git worktree move <old> .worktrees/<name>` 搬进来；还在用的等它关票时清掉。迁移完成前
不要对仓库根跑 `cargo clean`。

preflight 默认只打一行（branch / ahead-behind / dirty / worktree 数）；只有排查时才加
`--all` 列出每个 worktree。看到 `on main` 就先建 worktree。

共享工作区（仓库根目录）可能停在任意分支，甚至落后 main 几十个提交。读代码、grep、
取「main 的现状」之前先确认在哪个 worktree：`git -C <worktree> rev-parse --abbrev-ref HEAD`，
或直接用 `git -C <worktree>`。根目录的文件不是 main 的权威副本。

## 输出预算

每次工具输出都会长期占用上下文，默认走摘要：

- **票面**：`deck_issue_get` 用默认 excerpt 或 `section`；完整正文只在实现前读一次，
  不要把 28 张票的 full body 都拉进来。
- **测试**：用 `node tools/check.mjs quick`（编译）或 `test <filter>` / `full`；
  它只打印每个 target 的 `test result` 和失败尾部，不把几千行 cargo 输出带进上下文。
- **guard 工具**：preflight / merge-guard / closed-tickets 默认都是 brief；
  `--all`、`--verbose`、`--json` 只在需要机器处理或定位时用。
- **文件**：用带 `offset`/`limit` 的读取工具，不整文件读（本 DSH 环境是
  `ops_inspect_local_file`；搜索用 `ops_grep`，找路径用 `ops_glob`）。整文件重写用
  `write`/`edit`。读取结果明显被截断时，不要拿它回写。

## 共享 main 上不要做

- `git checkout/switch`：会换掉共享工作区，别的 session 的未提交改动会消失。
- `git reset --hard` / `git clean -fd`：未跟踪的 `src/server/control/` 就是这么消失的。
- `cargo clean`（在仓库根/共享 main 上）：会删掉 `target/`，包括历史遗留的 `target/wt-*`；
  清构建产物前先 `git worktree list` 确认没有仓库内的 worktree。
- 直接实现或 amend：实现放外部 worktree，主工作区只 fetch / merge / 审计。

看到不属于你的 dirty 文件：不要提交、不要清理；先看 `agent-preflight` 和
`git worktree list`。

## 提交与合并

- 一张票只开一个 worktree；CI 修复、review 收口都在同一条分支上追加 commit，
  不要为每个猜测新建 worktree（会多出 merge/push 步骤和 `AGENTS.md` 重载）。
- 在 worktree 里小步 commit，尽早 push 分支。
- 多行或含反引号/`$` 的提交信息用 `git commit -F <file>`；`-m` 会过 shell，反引号会被
  命令替换吃掉。
- main 的 worktree 通常就是仓库根目录：`git worktree list` 里找 `[main]` 那条。集成用
  `git -C <main-wt> merge --ff-only origin/main` 追上，
  再 `git -C <main-wt> merge --no-ff <branch> -m "merge #<ticket>: <一句话>"`，
  验证后 `git -C <main-wt> push origin main`。合并后的票分支保留。
- 合并前 `node tools/merge-guard.mjs <branch> --strict --ci`（brief；`--all` 才列全部重叠文件）。
  `--ci` 只读 main 最近一次 `test` workflow：如果 main 已经红，先修或说明为什么仍要合；
  它不会启动 CI。
- stale 分支合并后跑 `node tools/check.mjs full` + `node tools/closed-tickets-merged.mjs --map 112`；
  目标票测试绿不代表早先票的逻辑还在。
- 禁止在 main 上直接 commit；merge commit 与 fast-forward 不受限制。

## 关票与 worktree 收尾

关票是收尾的最后一步；只把 `feat/<ticket>` push 了不算做完。顺序固定：

1. **合入 main**：按上一节 `merge --no-ff`（或 fast-forward）进 main，main 上必须有该改动，不能只在票分支上。
2. **推送远程**：`git -C <main-wt> push origin main`；用 `git ls-remote origin refs/heads/main` 确认新 merge 已经在远端。
3. **用 main 验证**：在合并后的 main 上跑受影响的 target；跨模块或用户要求时跑 `full`；Windows 宿主的 Linux 半按 `docs/testing.md` §6 跑 WSL。
4. **清 worktree**：`git worktree remove <worktree>`（路径由 `worktree-add` 打印）。有未提交改动先处理，不要 `--force` 丢掉；合并后的票分支可以保留作记录。历史遗留的 `target/wt-*` 先 `git worktree move` 到仓库外或直接 `remove`，再清构建产物。
5. **最后关票**：`deck_issue_patch ... close=true` 或 `gh issue close`，票面写 merge commit 与验证结果。

只推分支、没合 main、没推远程、worktree 还在，都不算完成，不要关票。

## CI 边界

默认**不主动跑 GitHub CI**：本地先跑 `node tools/check.mjs quick` 与相关测试；全量
`full` 只在改动跨模块或用户要求时跑。只有用户明确要求“跑 GitHub CI / 全链路测试”时才触发或
等待 Actions。

用户要求看 CI 时：

- 每轮只起一个后台 `gh run watch <id> --exit-status`，做本地工作的同时在轮次结束时收一次结果；
  不要反复 `gh run list` / 轮询。
- CI 失败先跑 `node tools/ci-failures.mjs <run-id>`（只打印失败测试、panic 上下文、daemon log 与
  HTTP 响应探针），拿到真实原因再改代码；不要用一轮 CI 验证一个猜测。
- 同一个失败最多两轮修复；第二轮仍不绿就停下来重新诊断，而不是继续 push。
- `tests/http.rs` 的 `wait_until` 类 helper 超时会带 daemon log；新增等待 helper 也走它。

## 本仓库 hook

```sh
node tools/install-hooks.mjs
```

`.githooks/pre-commit` 拒绝 main 直接提交（merge 与 `SVCD_ALLOW_MAIN_COMMIT=1` 例外），
并对含 `.rs` 的提交跑增量 `cargo check`。`core.hooksPath` 是每 clone 本地配置，CI 不受影响。

## 票面移交（handoff）

票里写“留给 #NNN”或“后续切片”时，在 `## Handoff` 逐条记 `项 | 目标票 | 关闭前核对`。
**没有目标票的，先建 tracking issue**（无主 deferral 总表见 #128），把票号写进 Handoff；
不允许只写 `no-ticket` 留在正文。目标票关闭前用 `closed-tickets-merged.mjs` 验一遍。
