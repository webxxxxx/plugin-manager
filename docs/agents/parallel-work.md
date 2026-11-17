# 并行开发的 worktree 规则

这个仓库经常有多个 agent 同时改同一张地图（#112 S4）。共享 main 工作区曾被并行 session 的
`git checkout` / `git reset` / `git clean` 清掉过未提交文件，也让“票已关闭但分支未合并”的
#119 一直没人发现。规则很简单：**main 只做集成，实现永远在自己的 worktree 里做。**

## 每次开工

```sh
node tools/agent-preflight.mjs --fetch
git worktree add target/wt-<ticket> -b feat/<ticket> origin/main
```

`agent-preflight` 会打印当前 branch、所有 worktree、origin/main 的 ahead/behind、脏文件数。
看到 `current branch is main` 就先建 worktree；看到别的 worktree 不要去碰。

## 在共享 main 上不要做

- `git checkout <branch>` / `git switch`：会换掉共享工作区，其他 session 的未提交改动会消失。
- `git reset --hard` / `git clean -fd`：未跟踪的 `src/server/control/` 就是这么消失的。
- 直接实现或 amend：实现放在 `target/wt-<ticket>`，主工作区只用于 fetch / merge / 审计。

如果你在共享 main 上看到不属于你的 dirty 文件，**不要提交、不要清理**；先看
`node tools/agent-preflight.mjs` 和 `git worktree list`。

## 提交与合并

- 在 worktree 里小步 commit，尽早 push 分支：未提交的实现随时可能被并行操作带走。
- 合并前跑 `node tools/merge-guard.mjs <branch> --strict`；它列出分支落后 main 多少、两边
  都改过的文件（冲突/丢失的高风险面）。
- stale 分支合并后必须跑全量测试 + `node tools/closed-tickets-merged.mjs --map 112`；
  目标票测试绿不代表早先票的逻辑还在（stale base 会漏掉后来的修复）。
- 禁止在 main 上直接 commit；merge commit 与 fast-forward 不受限制。

## 本仓库 hook

```sh
node tools/install-hooks.mjs
```

装好后 `.githooks/pre-commit` 会：

1. 拒绝 main 上的直接提交（merge commit 除外；紧急情况用 `SVCD_ALLOW_MAIN_COMMIT=1`）；
2. 对含 `.rs` 的提交跑增量 `cargo check`，在 commit 前抓住截断/语法错误。

`core.hooksPath` 是每 clone 的本地配置，CI 不受影响。

## 整文件重写

`read` 默认只回前 2000 行。用它的输出重写大文件会把文件截断（本会话发生过 9 次
`unclosed delimiter`）。整文件重写用 `fs.readFileSync`（run_code）或 `edit`；`read` 的
`totalLines` 大于返回行数时，永远不要拿它回写。

## 票面移交（handoff）

票里写“留给 #NNN”或“后续切片”时，在 `## Handoff` 里逐条记：

```text
| 项 | 目标票 | 关闭前核对 |
|---|---|---|
| blob 上传并入共享预算 | #119 | 上传测试 + 无 64/429 计数 |
```

目标票关闭前用 `closed-tickets-merged.mjs` 验一遍；没有目标票的写 `no-ticket: 原因`，不要
只留在正文里。
