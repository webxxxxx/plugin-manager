# 单一 multicall 守护进程，PM 并入 svcd

系统原本把进程监督（svcd）与插件管理（PluginManager）分成两个角色，导致同一插件进程的端点、生命周期、路由表与工具表分属两个状态源。我们把插件视为「带准入策略的服务」，让 svcd 成为唯一监督者与插件管理器；交付物是一个 multicall 单二进制，链接出 `svcd` / `svc` / `plugin` / `mcp` 四个入口（`plugin` 与 `svc` 同源，但只作用于插件）。每个服务有生命周期与能力面：死的只暴露 `<id>.start`，活的才把 manifest 声明的能力放进 `tools/list`。代价是插件管理缺陷与监督器同生命周期；收益是消除双监督者的状态分叉，并把双二进制约 921.5KiB 的预算压到单二进制（估 550–600KiB，待实测）。

## Considered Options

- 保留双进程（svcd + PM）：故障隔离更好，但端点交接、路由表/工具表同步、依赖图分裂，且体积多约 300KiB。
- 单进程但 PM 作为内部服务：仍保留一个「服务中的服务」的间接层，没有收益。
- 单二进制 multicall（选中）：四个入口共享一套注册表与依赖图，插件管理成为 svcd 的内部功能。

## Consequences

- PluginManager 不再是服务或保留名；地图的前提与 Destination 已按单二进制改写。
- `svc list` 与 `tools/list` 同源；工具面随服务生死动态变化，可见性机制归 MCP 聚合层。
- 插件控制通道默认子进程 stdio NDJSON；带外通道只能作为旁路。
- 体积预算分配与 CI 尺寸门禁需按单二进制重算。
