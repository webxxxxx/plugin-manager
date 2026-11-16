## Agent skills

### Issue tracker

Issues and specs live in this repo's GitHub Issues, driven by the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five triage roles: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

This repo is single-context: one `GLOSSARY.md` at the repo root and ADRs in `docs/adr/`. See `docs/agents/domain.md`.

## 决策流程

让我做决策之前，先给出「执行流程图」：用 Mermaid `flowchart` 画出**被决策对象在真实调用链/数据流中的位置**，而不是项目计划图。图中必须标出：

- 它前面经过哪些阶段、后面流向哪里（上游/下游各一段，不只画它自己）；
- 当前决策点：用 `:::decision` 高亮并注“当前决策”；
- 已接线与未接线的边要区分（实线/虚线），避免把只存在于测试的路径画成生产路径；
- 每条候选路径从决策点出发后的下一步与终点，以及代价/风险。

规则适用于所有需要我拍板的场合，包括 `ask_user_question` 提问。没有执行流程图不要提问。

本 GUI 已启用 Mermaid 渲染，直接用 ```mermaid 代码块画执行流程图即可；不要为流程图额外生成 PNG 或做本地转换。图中仍需标出调用链上下游、当前决策点、已接线/未接线。

## 实现标准

除非任务明确说明只是原型、演示或一次性验证，默认直接实现生产级代码：

- 处理错误路径、边界条件、并发/背压和资源释放；
- 新行为要有测试，跨平台代码分别验证；
- 不把测试桩、伪实现或“仅本地可用”的路径当作完成；
- 配置项写入 `svcd.conf.example` 和生成配置的注释；
- 改动完成后跑对应平台测试并报告结果。

