# 并行开发规则

这个仓库经常有多个 agent 同时改同一张地图（#112 S4）。共享 main 曾被并行 session 的
`git checkout` / `git reset` / `git clean` 清掉过未提交文件。规则：**main 只做集成，
实现永远在自己的 worktree 里做。**

## 开工

```sh
node tools/agent-preflight.mjs --fetch
git worktree add target/wt-<ticket> -b feat/<ticket> origin/main
```

preflight 默认只打一行（branch / ahead-behind / dirty / worktree 数）；只有排查时才加
`--all` 列出每个 worktree。看到 `on main` 就先建 worktree。

共享工作区（仓库根目录）可能停在任意分支，甚至落后 main 几十个提交。读代码、grep、
取「main 的现状」之前先确认在哪个 worktree：`git -C <worktree> rev-parse --abbrev-ref HEAD`，
或直接用 `git -C target/wt-<ticket>`。根目录的文件不是 main 的权威副本。

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
- 直接实现或 amend：实现放 `target/wt-<ticket>`，主工作区只 fetch / merge / 审计。

看到不属于你的 dirty 文件：不要提交、不要清理；先看 `agent-preflight` 和
`git worktree list`。

## 提交与合并

- 一张票只开一个 worktree；CI 修复、review 收口都在同一条分支上追加 commit，
  不要为每个猜测新建 worktree（会多出 merge/push 步骤和 `AGENTS.md` 重载）。
- 在 worktree 里小步 commit，尽早 push 分支。
- 多行或含反引号/`$` 的提交信息用 `git commit -F <file>`；`-m` 会过 shell，反引号会被
  命令替换吃掉。
- main 的 worktree 不一定是仓库根目录：`git worktree list` 里找 `[main]` 那条
  （通常是 `target/wt-merge-*`）。集成用
  `git -C <main-wt> merge --ff-only origin/main` 追上，
  再 `git -C <main-wt> merge --no-ff <branch> -m "merge #<ticket>: <一句话>"`，
  验证后 `git -C <main-wt> push origin main`。合并后的票分支保留。
- 合并前 `node tools/merge-guard.mjs <branch> --strict --ci`（brief；`--all` 才列全部重叠文件）。
  `--ci` 只读 main 最近一次 `test` workflow：如果 main 已经红，先修或说明为什么仍要合；
  它不会启动 CI。
- stale 分支合并后跑 `node tools/check.mjs full` + `node tools/closed-tickets-merged.mjs --map 112`；
  目标票测试绿不代表早先票的逻辑还在。
- 禁止在 main 上直接 commit；merge commit 与 fast-forward 不受限制。

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
