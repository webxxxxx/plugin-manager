# exec 服务的默认载体是监督槽

## Status

Accepted。细化 [ADR-0013](0013-service-contract-interface.md)。

## Decision

- 一个服务只要声明了 `exec`，它的运行时资源就由监督槽（`Slot`）唯一拥有；`tools/call` 在调用点构造 `SlotWire(handle, client)`，不经过 `Launchers`。
- `ImplRef{tag, binding}` 只用于槽之外的载体：内置 `svc`（`tag=builtin`），以及未来的复合/远程。没有 `tag=process`；process 不是第二种载体，只是槽里的一个进程。
- `SlotWire` 只拥有调用面：`Ensure` / `Quiesce` 以 `implementation_error`/`never` 拒绝，`Probe` 读槽的粗粒度健康；生命周期动作由 daemon 执行，不经 Wire。
- 本决策要求 `exec` 与 `impl_ref` 互斥：前者选槽，后者选 launcher；两者都没有的服务不可调用。装配点在注册表边界强制这条不变式（`Service::carrier`，`Registry::insert` 与 `Registry::replace`，见 #109）：加载器把拒绝转成 `LoadIssue`，安装以错误返回，reload 记日志跳过。

## Considered Options

- **把 `tag=process` 重新定义为槽视图**（binding=service id）：需要第二张 `SlotHandle` 映射并与 daemon 的 `slots` 同步，还要给 launcher 加 per-call progress sink；只为了统一路径，收益不抵成本。
- **新增 `tag=slot`、保留 `process` 给未来无状态进程**：两个 tag 指同一件事，且 `process` 没有调用方。
- **保留 `ProcessWire` 作为未来 adapter**：它是第二个 spawn/Drop-kill 所有者，留着就有被误接线的风险；删除让双所有者不可能发生。

## Consequences

- 删除了 `ProcessLauncher` / `ProcessWire` / `ProcessSpec`；process adapter 的 conformance 覆盖随之消失。一致性断言按**入口**划分（#109）：`SlotWire` 过调用面那一套（声明的失败类别不被掩掉、取消后不得返回 `Ok`），生命周期那一套换成委托断言（必须以 `implementation_error`/`never` 拒绝 Ensure / Quiesce，Probe 仍作答）；槽自己的生命周期行为仍由 slot / daemon 测试覆盖。未来真需要无状态进程载体时再加回。
- 新增运行载体仍只加一个 adapter 与一个 tag；但「槽之外」的载体才走 `Launchers`，`exec` 是槽的内建默认。
- `ImplRef` 不进入外部 `services.d` / `.spk` schema；等复合/远程有真实调用方再引入。
