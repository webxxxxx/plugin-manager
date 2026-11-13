# schema 2：插件生命周期与解释器依赖

core drop-in 脚本已经有 `start/stop/status` 与「从脚本声明推导生命周期」的执行路径，但 `.spk` 插件只有协议进程：manifest 不能声明生命周期，安装路径也把 `Service.lifecycle` 写死为 `None`。结果是一个普通服务脚本打成 `.spk` 后只能当 notify 插件、等 `svcd.ping` 超时，或者手写一个没有 stop/status 的 `exec` 插件。我们让 manifest 成为签名契约：`schema: 2` 增加 `lifecycle` 与 `interpreter`，安装/扫描把它们映射到与 core 完全相同的 `Service` / `Slot` 执行路径。

## Decision

- `schema: 2` 的 manifest 可以声明 `lifecycle: {start?, stop?, status?}`；路径是包内相对路径、必须在 `files[]`。语义与 core 一致：`start` 替换 `exec` 作为被监督前台进程，`stop` 在终止前运行，`status` 只在 probe 时运行，超时默认/上限与 core 相同。
- 脚本 payload（不是 ELF / Mach-O / PE）必须声明 `interpreter: {name, args?, version?}`。`name` 是 PATH 上的裸名；`version` 是数字约束，为空/省略时**只校验存在，不执行版本探测**。
- 安装时把解释器解析为绝对路径并按需校验版本，失败则不落盘；扫描时只重新解析路径。运行时的 `exec` 与 lifecycle 钩子统一构造 `interp + args + [script] + action_args`。
- `capabilities` 只允许 `exec.ready=notify`；`exec` 模式没有 stdin，不可能承载 `tools/call`。
- `plugin bundle --from-core --interpreter <name>` 按脚本声明的动作生成 lifecycle，使 core 脚本「原样进包 + 补 manifest」即可转换。
- `schema: 2` 同时是兼容门禁：老 daemon 在验包阶段拒绝，而不是静默忽略新字段。

## Considered Options

- **安装时从脚本内容推导 lifecycle（拒绝）**：与 core drop-in 最像，但行为不钉在签名 manifest 上，且难以区分「普通脚本」与「恰好有 `start()` 函数的协议脚本」。
- **lifecycle 只归 core（拒绝）**：保持原边界，但无法把 core 脚本打成可版本化、可热安装、可导出的 `.spk`。
- **shebang 权威、manifest 只做依赖声明（拒绝）**：Unix 可行，但 Windows 的 `.py` / `.js` 没有可执行 shebang 路径，且安装校验的版本与运行时实际解释器可能分叉。
- **schema 1 加法字段（拒绝）**：老 daemon 会照装，但忽略 lifecycle/interpreter，脚本可能以错误参数启动而不报错。

## Consequences

- 老 daemon 不能安装 schema 2 包；schema 1 旧包行为不变。
- 解释器版本探测是安装期以安装者权限执行的子进程：只用裸名 + 超时 + 记录的探测命令，且版本为空时完全不探测。Windows 上解释器必须是 `.exe` / `.com`，`.cmd` / `.bat` shim 不被接受，避免 spawn 依赖 shell。
- `plugin status` 与 `svc status` 一样带 probe，脚本插件的 status 动作才有出口；`plugin start/stop` 只查 kind，不触发 probe。
- Windows 没有 SIGTERM 的 polite 路径：`stop` 钩子结束后直接终止进程树，不再空等 10 秒停止窗口。
- 纯 native 插件仍不需要解释器；混合包按文件判定，native 直接执行、脚本走解释器。
- 未来若要给原生二进制也声明 lifecycle 或按 hook 覆盖解释器，可以在 schema 2 内加法扩展，不需要再动执行器。
