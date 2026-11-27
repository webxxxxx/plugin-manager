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

## 修订（2026-11-23，#260）

manifest 格式重编号为唯一的 `schema: 1`，本 ADR 的两条结论随之调整：

- **`lifecycle` 与 `interpreter` 不再是 schema 2 的专利**：字段本身、语义与执行路径不变，只是不
  再挂在版本号分级上；「脚本 payload 必须声明解释器」这条**取消**。理由：执行侧本来就支持两条路
  ——带 shebang 的脚本在 Unix 直接 exec、`.cmd` / `.bat` 在 Windows 经 `COMSPEC`——强制声明会
  拒绝本来能跑的包。声明了解释器就仍然统一走 `interp + args + [script] + action_args`，并且此时
  必须真有脚本可跑（否则是无效声明，拒装）。
- **没有「老 daemon 拒绝」这道门了**：旧线的包（任何版本）都缺必填的 `id`，验包阶段直接拒绝并
  提示用 `plugin bundle` 重打包；不做读入兼容。
- shebang 权威与解释器权威的分叉仍在：声明了就用声明的解释器，不声明就走 shebang / `.cmd`。

## 修订（2026-11-26，#323）

生命周期语义翻转，两条结论随之改写：

- **`start` 不再是被监督进程。** `exec` 是唯一的被监督进程；`lifecycle.start` 改为「前置准备」：每次
  spawn `exec` 之前运行，必须退出，非零或超时即本次启动失败、`exec` 不 spawn。理由：`start` 同时当
  「进程」和「准备动作」用，逼着插件写一个永不执行的影子 `exec`（`sshd` 的 `exec.args` 为空，真跑起来
  会打印 usage 并 `exit 2`），而且准备失败与进程崩溃无法区分——两者在 `ready: notify/exec` 上都表现为
  「进程退出了」。
- **`stop` 改为进程消失之后的收尾，不再在终止前运行。** 终止权归宿主（Unix 进程组 `SIGTERM` → 10 秒 →
  `SIGKILL`；Windows 直接强杀），`stop` 跑的时候进程已经不在；`Stopped` 以它返回为准。理由：同一个
  `stop` 之前在停止路径上当终止器、在崩溃路径上当清理器，两种契约迫使插件写两套防御；而且状态在
  leader 退出时就结算，`svc stop` 会在钩子还没跑完时返回，紧接着的 `svc restart` 会把钩子掐死（#323）。
  代价是 Windows 上没有「先问再杀」的优雅停机；当前生命周期插件都是 Linux 专用，真要支持应由 SCM
  控制通道承担，而不是加回钩子。

`lifecycle.status` 与超时默认值不变；`plugin bundle --from-core` 与 core 脚本推导同步改成把声明的
`start` 写进 `exec.args`。

补充（#324）：`stop` 在「实例不会再回来」的每条路径上都跑——主动停、崩溃且不重启、崩溃且要重启、
启动失败；服务只有在它返回之后才报 `Stopped`。

## 修订（2026-11-27，#335 / #337）

生命周期多了一个窗口，就绪多了一种语义，结论随之补充：

- **`lifecycle.started`**：Ready 之后、`Running` 之前跑一次，必须退出；非零或超时即本次启动失败
  （停整组、跑 `stop`、按 `restart` 退避）。每次 spawn（`svc start`、`svc restart`、崩溃重试）都重跑，
  所以它必须幂等。声明了它就不得同时声明 `remain_after_exit`：一次性服务的进程可能在门控期间退出，
  没有可门控的实例（定义加载即拒绝）。它补的是 `start`（spawn 之前）与 `stop`（进程消失之后）之间的
  空窗口；「每个状态一个回调」的形态仍然不做。
- **`stop` 的停止原因**：`stop` 钩子额外拿 `SVCD_STOP_REASON`（`stop` / `shutdown` / `crash` / `exit` /
  `start-failed`）与 `SVCD_LAST_EXIT`（只有 `crash` / `exit` 时是退出码）。一个钩子按原因分支，不再需要
  为每个状态配一个回调。
- **`exec.ready: "port"`**：就绪也可以是「宿主连得上声明的端口」；`exec.port` 必填、`exec.probe_addr`
  默认 `127.0.0.1`。它和 `exec` 一样不建协议通道，因此不是通道对端，能力只能挂一次性的 `hook`（见下方
  2026-11-28 的修订）。spawn 前
  宿主先试绑同一地址，端口被别人占着就直接失败；这是探测不是保证（预检与 connect 之间仍有 TOCTOU 窗口）。

## 修订（2026-11-28，#342）

就绪不再建通道之后，能力面少了一条投递路径，补上一格，结论随之补充：

- **`capabilities[].hook`**：一个能力可以由一次短命动作作答（`{path, args?, env?, timeout?}`）。宿主每次
  `tools/call` 起一次该动作：参数写 stdin，stdout 是这次调用的文本结果——与通道那侧 handler 的 stdout
  同形，所以页面 `invoke()` 与 MCP 客户端在两条路径上拿到的形状一致。非零退出、超时、取消都是这次调用
  的失败，不是服务失败（不标降级）。
- **一个能力只有一种投递方式**：`ready: notify` 的通道对端不许声明 `hook`（通道已经投递了）；没有通道的
  `exec` / `port` 插件必须给**每个**能力声明 `hook`，否则那是一个永远答不上来的工具。
- 它和 `started` 是同一个形状的两个位置：`started` 是「有一个窗口要跑一次性动作」，钩子能力是「有一次
  调用要跑一次性动作」。「每个状态一个回调」仍然不做。
