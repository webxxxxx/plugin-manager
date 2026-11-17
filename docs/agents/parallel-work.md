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

## 输出预算

每次工具输出都会长期占用上下文，默认走摘要：

- **票面**：`deck_issue_get` 用默认 excerpt 或 `section`；完整正文只在实现前读一次，
  不要把 28 张票的 full body 都拉进来。
- **测试**：用 `node tools/check.mjs quick`（编译）或 `test <filter>` / `full`；
  它只打印每个 target 的 `test result` 和失败尾部，不把几千行 cargo 输出带进上下文。
- **guard 工具**：preflight / merge-guard / closed-tickets 默认都是 brief；
  `--all`、`--verbose`、`--json` 只在需要机器处理或定位时用。
- **文件**：`read` 带 `offset`/`limit`，不整文件读；整文件重写用 `fs.readFileSync`
  或 `edit`。`read` 的 `totalLines` 大于返回行数时，永远不要拿它回写。

## 共享 main 上不要做

- `git checkout/switch`：会换掉共享工作区，别的 session 的未提交改动会消失。
- `git reset --hard` / `git clean -fd`：未跟踪的 `src/server/control/` 就是这么消失的。
- 直接实现或 amend：实现放 `target/wt-<ticket>`，主工作区只 fetch / merge / 审计。

看到不属于你的 dirty 文件：不要提交、不要清理；先看 `agent-preflight` 和
`git worktree list`。

## 提交与合并

- 在 worktree 里小步 commit，尽早 push 分支。
- 合并前 `node tools/merge-guard.mjs <branch> --strict`（brief；`--all` 才列全部重叠文件）。
- stale 分支合并后跑 `node tools/check.mjs full` + `node tools/closed-tickets-merged.mjs --map 112`；
  目标票测试绿不代表早先票的逻辑还在。
- 禁止在 main 上直接 commit；merge commit 与 fast-forward 不受限制。

## 本仓库 hook

```sh
node tools/install-hooks.mjs
```

`.githooks/pre-commit` 拒绝 main 直接提交（merge 与 `SVCD_ALLOW_MAIN_COMMIT=1` 例外），
并对含 `.rs` 的提交跑增量 `cargo check`。`core.hooksPath` 是每 clone 本地配置，CI 不受影响。

## 票面移交（handoff）

票里写“留给 #NNN”或“后续切片”时，在 `## Handoff` 逐条记 `项 | 目标票 | 关闭前核对`；
没有目标票的写 `no-ticket: 原因`。目标票关闭前用 `closed-tickets-merged.mjs` 验一遍。
