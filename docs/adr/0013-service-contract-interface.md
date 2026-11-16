# S1 服务契约：一个值 + 两个入口

> 槽承载的 `exec` 服务是「实现由 `ImplRef` 指定」的例外；见 [ADR-0015](0015-exec-services-default-to-slot.md)。

## Decision

- **契约是一个可序列化的值** `Contract`：id、能力面、义务（健康/生命周期/一次性/限额）、它可能给出的失败类别。**不含进程、pid、端口、重启次数。**
- **运行实现只有两个入口**：`call(target, Invocation) -> Result<Reply, Failure>` 与 `lifecycle(target, Ensure | Quiesce | Probe) -> Result<Settled, Failure>`。健康不是第三个入口，也不是保留能力——它走 `Probe`，因为提问者、权限与时间尺度都和普通调用不同。
- **实现由 `ImplRef{tag, binding}` 指定**，装配点注册 `tag → adapter`。新增运行载体只加一个 adapter 与一个 tag，不改内核。
- **能力面显式声明、可枚举、稳定**：`Capability{name, desc, input_schema, effect: Pure|Read|Write, idempotent}`。三处投影（MCP `tools/list`、插件 UI `/rpc`、CLI `svc call`）只读它，不各自维护表。
- **先验可重试是一等事实**：`effect` 与 `idempotent` 在能力上标注；`Invocation` 可带 `idempotency` 键与 `trace`。事后补不上，因为每个能力都要重标一遍。
- **失败是 `{class, retry: Now | Never | After(t), detail}`**；`detail` 只给人看，上层不得解析。类别必须落在契约声明的 `failures` 子集内，**越界归一为 `ImplementationError`**。
- **不在能力面里的能力：内核就地拒绝**，不产生调用、不产生失败类别。
- **同一 `(调用方, 能力)` 有序，跨能力无序**；取消尽力而为，迟到结果按 `idempotent` 丢弃或上报；`Cancelled` **只用于调用方主动取消**。
- **死亡与超时按判定时刻的客观状态归类**：进程已死 → `NotRunning`；进程仍活且到点 → `Timeout`。超时由**内核**计时，默认与上限来自配置（`default_timeout` / `max_timeout`），全局常量 `CALL_TIMEOUT_MS` 退役。
- **就绪前做一次 `Probe`**：超时或实现不认这个 id，记为「未就绪」，继续等到 `start_deadline` 再判启动失败；契约只暴露**服务级**健康，不暴露逐能力状态。
- **内联上限 1 MiB**，大内容只传 `$blob` 引用；三种线格式（NDJSON 行帧 / 4 字节长度帧 / raw 8 字节帧）**不进契约**。raw 是 UI↔插件直通，不属于本契约。
- **每个 adapter 必须通过它所在入口的一致性断言**：8 类失败、1 MiB 边界、取消顺序、能力面稳定属于调用面；健康三态与 `Ensure` / `Quiesce` 属于生命周期入口——自己拥有它的 adapter 直接断言，把生命周期委托出去的 adapter（槽，见 [ADR-0015](0015-exec-services-default-to-slot.md)）必须明确拒绝而不是假装做到。

## Consequences

- 今天的 `Slot::request_capability`（六个参数、绑在进程载体上、`CallError` 只有四个变体）退役；失败从「一句英文」变成可判定的类别。
- 超时/死亡竞态消失：类别只取决于判定时刻的状态，不取决于事件处理顺序。
- 第三方脚本仍可能「声明了没实现」：`Probe` + `failures` 子集 + 越界归一挡住大部分，能力级不一致只能靠首次调用暴露——这是弱契约的已知代价，不假装能消除。
- **插件作者零负担**：manifest 与现有 `tools/call` 循环不变；变重的是我们自己的四个 adapter。
- 契约必须最保守：任何「某个实现专属」的字段都不得进入，否则其余实现永远补不齐。
- 能力面的增加与改名是**破坏性变更**：三处投影与编排的映射都依赖它。
- 健康语义从「进程在不在」升级为四态（活着 / 就绪 / 降级 / 死亡），监督的重启判定要按新语义重写，而不是继续读退出码。
