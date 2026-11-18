## Agent skills

### Issue tracker

Issues and specs live in this repo's GitHub Issues, driven by the `gh` CLI. See `docs/agents/issue-tracker.md`.

### 并行开发 / worktree

每个票在自己的 worktree 里做；worktree 放仓库内 `.worktrees/`（已 git 忽略，默认 `.worktrees/<票号>`），不要放 `target/`（`cargo clean` 会连它一起删）；共享 main 只做集成。开工前读 [docs/agents/parallel-work.md](docs/agents/parallel-work.md)。关票前先把代码合入 main、推送远程，并清掉该票 worktree；只推分支不算完成。

### Triage labels

Default five triage roles: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

This repo is single-context: one `GLOSSARY.md` at the repo root and ADRs in `docs/adr/`. See `docs/agents/domain.md`.

## 决策流程

要我做决策、或你要用 `ask_user_question` 提问前，先按 [docs/agents/decisions.md](docs/agents/decisions.md) 画执行流程图；那份文件写明图里必须标什么、哪些边算已接线。

## 实现标准

除非任务明确说明只是原型、演示或一次性验证，默认直接实现生产级代码：

- 处理错误路径、边界条件、并发/背压和资源释放；
- 新行为要有测试，跨平台代码分别验证；
- 不把测试桩、伪实现或“仅本地可用”的路径当作完成；
- 配置项写入 `svcd.conf.example` 和生成配置的注释；
- 改动完成后跑对应平台测试并报告结果；Windows 宿主的 Linux 半用 `node tools/check.mjs linux` 真跑（细节见 `docs/testing.md` §6），`cross` 只算编译。
- **有界队列/背压**：新增、删除或修改 bounded channel 时必须带满队列测试；删除一条背压路径时，在同一处说明替代背压。
- **跨平台测试**：行为测试必须平台中立；`/proc`、Win32 等 OS 细节测试只能做补充，不能替代另一平台的验证。
- **测试 helper**：不得伪造生产失败类别；实现没回调、没结果时直接失败，不要编一个 `Timeout` / `NotRunning`。
- **提交边界**：行为改动与 dead-code 清理、`#[cfg(test)]` gating 分开提交；review 的 Spec 轴会把混入的清理列为 scope creep。
- **宽改**：重命名 / 改类型 / 搬符号这类「一处机械改、几百个调用点」先读 `docs/agents/wide-refactor.md`，用 `node tools/codemod.mjs` 的清单 + 编译门，搬整块 item 用 `node tools/move-item.mjs`；不要手写 `str.replace` 或提取脚本。
- **guardrail 改动**：改 `dep-lint` / `check.mjs` / `merge-guard` 这类检查工具，必须附**注入测试**——先证明新增检查在坏输入上会红，再证明恢复后绿，并把注入方式写进票面；没有注入测试按未完成处理。
- **文档一致性**：改 `docs/architecture.md` 的「当前/下一步」时同步 §8 seam 表；`tools/arch-map.json` 的过期/缺失映射与 stdio seam 不变量由 `dep-lint` 检查（CI `guardrails` job 会跑）。
- **CI 边界**：除非用户明确要求 GitHub CI 或全链路测试，不主动触发、等待或观察 GitHub Actions；先用本地 `node tools/check.mjs`。

