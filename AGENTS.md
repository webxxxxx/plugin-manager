## Agent skills

### Issue tracker

Issues and specs live in this repo's GitHub Issues, driven by the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five triage roles: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

This repo is single-context: one `GLOSSARY.md` at the repo root and ADRs in `docs/adr/`. See `docs/agents/domain.md`.

## 实现标准

除非任务明确说明只是原型、演示或一次性验证，默认直接实现生产级代码：

- 处理错误路径、边界条件、并发/背压和资源释放；
- 新行为要有测试，跨平台代码分别验证；
- 不把测试桩、伪实现或“仅本地可用”的路径当作完成；
- 配置项写入 `svcd.conf.example` 和生成配置的注释；
- 改动完成后跑对应平台测试并报告结果。

