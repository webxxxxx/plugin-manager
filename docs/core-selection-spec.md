# 核心层技术选型规格

> 状态：决策层定稿（2026-11-12）
> 依据：地图「核心层技术选型」的 26 张已关子票、8 份研究笔记、ADR 0001
> 范围：svcd / svc / mcp 核心层；不含商城、ipk2spk、AI 建插件流水线
> 用法：§1–§12 是规格；**§13 是冲突与缺口，交接前必读**

## 0. 一句话

一个 Rust 写的 multicall 二进制，链接出 `svcd`（守护进程）/ `svc`（服务 CLI）/ `plugin`（插件 CLI）/ `mcp`（MCP 桥）四个入口；插件是 `.spk` 包，由 svcd 以 root / System 身份监督；核心层不链 TLS、不打包 WebView 引擎，嵌入式 ≤ 2,000,000 B、桌面 ≤ 30,000,000 B（均指核心二进制）。

## 1. 系统模型

### 1.1 交付物

- **一个 crate、一个二进制、argv[0] 派发**；安装时建 `svcd` / `svc` / `plugin` / `mcp` 四个硬链接，失败降级 symlink，再降级 copy 并告警。
- 桌面另有 `svc ui` 子命令作为 GUI 入口，不单独占入口名。
- **PluginManager 不再存在**：不是进程、不是服务、不是保留名。插件管理是 svcd 的内部功能。
- `svcd run --foreground` 供服务管理器拉起；`svcd install-service / uninstall-service` 向宿主注册自身（systemd / launchd / SCM / BusyBox inittab / procd 五份计划）。
### 1.2 服务与能力面

- 每个服务有 id、kind（core / plugin）、生命周期、依赖与能力面。注册表 = 内置 `svc` + `services.d/*.json` 的 core + `<data>/plugins/*/current` 的 plugin；没有第二份清单。core 与 plugin 的来源、信任、安装方式见 §1.6。**能力面只归 plugin**：core 是生命周期单元，不参与 `tools/call`。
- `svc list` 与 MCP 的 `tools/list` **同源**：死的服务只暴露 `<id>.start`；活的才把 manifest 声明的能力放进工具面。
- `start` 同步到 ready；插件崩溃即摘能力，不自动启动（只有 enabled 的开机启动会重试）。
- `svc.` 命名空间保留给核心能力；插件能力对外是 `<serviceId>.<cap>`。

### 1.3 监督器

- 状态机：`Stopped | Starting | Running | Stopping | Backoff | Failed`，事件带 pid，迟到事件按 pid 丢弃。
- 退避：曾在 Running ≥ 10s → 清零 + 立即重启；否则 1/2/4/8/16/30s；连续 5 次进 `Failed`，只有显式 start 复位。
- 就绪：默认 probe-on-spawn（发首个协议请求，首个合法响应 = Ready），`exec.ready = "exec"` 可跳过；start 超时 30s。
- 停止：SIGTERM（进程组）→ 10s → SIGKILL → 5s → 记 unkillable；全局关机预算 30s，unit 侧 `TimeoutStopSec=40`。Windows：控制帧 + 关 stdin → 等 → `TerminateJobObject`（整个服务树）。
- **服务树即进程组**：Unix 每次 spawn 建独立进程组，Windows 每个服务一个 job object；停止/重启用整组终止。组内任一进程仍存活时服务保持 `Running`，leader 退出不触发重启；整组退出后才按 leader 的退出码进入重启策略。`notify` 类服务的 stdout EOF 仍按控制端点消失处理（立即停止）。
- **父进程死亡（2026-11-13，测试覆盖）**：svcd 异常退出（kill -9 / TerminateProcess）时子进程随父退出——Windows 用 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，所有被监督进程加入该 job），Linux 在 child 里 `prctl(PR_SET_PDEATHSIG, SIGTERM)`；重启后按 enabled 重新拉起，不会再与上次运行的实例并存。macOS 没有等价的廉价原语，仍是已知缺口，待后续票。
- 依赖：按层拓扑，同层并行、层间串行；依赖失败不级联停，只摘能力并拒绝新的 start；依赖恢复不自动拉起之前被挡住的依赖者。
- 环：安装与加载都拒绝，带环路径。

### 1.4 依赖

- `dependencies` 是服务 id 列表，**v1 不做版本约束**；任意已安装版本即满足。
- 安装允许先装依赖者；`start` 按拓扑拉起已安装的依赖，缺依赖才拒绝并列出 id。
- 升级/卸载不阻止、不级联停、不自动重启依赖者；能力面按 §1.2 摘除/恢复。
- 版本兼容由 manifest 的 `protocol` / `platform` 与插件自身负责；未来要范围再加 `id@range`，加法兼容。

### 1.5 服务安装

- 五份 `InstallPlan`：systemd（`/etc/systemd/system/svcd.service`，Type=notify、Restart=on-failure、TimeoutStopSec=40）、launchd（`/Library/LaunchDaemons/com.webxxxxx.svcd.plist`，KeepAlive、ThrottleInterval 10）、Windows SCM（`sc create` + failure actions）、BusyBox inittab（`::respawn:`）、procd（`/etc/init.d/svcd`）。
- 安装需 root / Administrator；幂等（原子替换 + EnsureLine；`sc create` 1073 视为已存在）。
- `svcd --install [--prefix] [--conf <file>] [--no-service] [--target]`：复制/硬链 `svcd`、`svc`、`plugin`、`mcp` 到平台默认目录（Unix `/usr/sbin`，Windows `%ProgramFiles%\svcd`），再调用 `install-service --program <安装路径>`；`--uninstall` 逆操作，默认保留 config/data/logs，`--purge` 才清。
- `svcd --install --conf <file>`：校验并原子复制到 `<config>/svcd.conf`。键：`http_port`（0 允许临时端口；`SVCD_HTTP_PORT` 仍覆盖）、`lan_default`（true 时写 `<config>/lan.json` 并生成/复用 token）、`plugins_dir`、`services_dir`。相对路径相对 `<config>`；未知键/坏值拒绝且不覆盖已安装配置；`--uninstall` 默认保留，`--purge` 删除。
- 服务启动配置优先级：`--conf`（`--install --conf` 记录到服务命令行）→ 程序目录 `svcd.conf` → `<config>/svcd.conf`（legacy）→ 生成默认（程序目录；不可写时用 `<config>/svcd.conf`）。默认含 `http_port = 8787`、`lan_default = false` 与平台 plugin/services 路径；模板见根目录 `svcd.conf.example`。
- 卸载默认保留 `<data>` 与配置，`--purge` 才删。

### 1.6 core 服务与 plugin 的边界

**core 服务是「直接声明的启动脚本或可执行文件」；plugin 是「标准 .spk 包」。两者是不同来源，不是同一包格式的两种模式。** 执行路径是共用的：schema 2 的 plugin 可以声明与 core 同形的 `lifecycle`，由同一个监督器跑 `start/stop/status`；区别仍在来源、信任与安装/配置 verbs，而不是「谁能有生命周期」。

| | core 服务 | plugin |
| --- | --- | --- |
| 来源 | 管理员在 `<config>/services.d/*.json` 或 `services.d/<id>.sh|bat|cmd|ps1` 声明的脚本 / 可执行文件；`svc` 是内置 core | `.spk` 包，经 `svc install` |
| 信任 | 无包签名；root 所有的定义文件 + 本机管理员 | Ed25519 manifest 签名 + `files[]` + `trust.d` |
| 安装/升级 | 由 OS 包管理器 / 管理员管文件；svcd 不安装、不升级 | `svc install`（升级同）/ `uninstall` |
| 配置 | 脚本/bin 自己管；svcd 不提供 `config` / `editconfig` / `export` | `config` / `configCheck` / `editconfig` / `.spkb` |
| 生命周期 | `start/stop/status`（JSON 定义或 drop-in 脚本） | schema 2 manifest 可声明同形 `lifecycle`；执行路径共用 |
| hooks | 无 | post-install / pre-remove |
| UI | 无 `ui`、无能力面 | `ui` + `/plugin-ui/{id}` |
| 注册表 | 启动时扫 `services.d/*.json` + 内置 `svc` | 扫 `<data>/plugins/*/current` |

`services.d/<id>.json`：

```json
{
  "schema": 1,
  "id": "my-core",
  "version": "1.0.0",
  "desc": "…",
  "protocol": 1,
  "exec": { "path": "/usr/local/bin/my-core", "args": [], "env": {}, "ready": "notify" },
  "dependencies": [],
  "lifecycle": {
    "start":  { "path": "/usr/local/bin/my-core", "args": ["start"],  "timeout": 30 },
    "stop":   { "path": "/usr/local/bin/my-core", "args": ["stop"],   "timeout": 10 },
    "status": { "path": "/usr/local/bin/my-core", "args": ["status"], "timeout": 5 }
  }
}
```

- `exec.path` 必须是绝对路径；Unix 可以是 shebang 脚本，Windows 是 `.exe` / `.cmd` / `.bat`；`ready` 同 plugin（默认 `notify`，脚本不讲话就用 `exec`）。
- `lifecycle` 全部可选；`start` 存在时它就是被监督的前台进程（替换 `exec`，`ready` 仍按 `exec.ready`；不讲话的脚本把 `exec.ready` 设为 `exec`），不得自行 daemonize；`stop` 在 SIGTERM 之前运行用于释放资源，超时/非零只记日志后继续停止序列；`status` 由 `svc status` 按需探测，stdout 作为 `details.probe` 的细节补充，退出码不改变服务状态。钩子 `path` 必须绝对，环境继承 daemon 并附加 `SVCD_SERVICE_ID` / `SVCD_ACTION` / `SVCD_DATA_DIR` / `SVCD_CONFIG_DIR` / `SVCD_LOG_DIR`。
- 文件 root 所有、0640；启动时读取，改定义后重启 svcd 生效（v1 不做热加载）。
- 不接受 `.spk` 专属字段：`platform` / `files` / `hooks` / `config` / `ui` 出现即定义非法；其他未知字段忽略。
- `id` 不能是 `svc`（保留给内置 core），且不能与 plugin id 冲突；冲突时 plugin 安装拒绝、core 定义加载拒绝。
- `<config>/services.d/<id>.disabled` 存在时该 core 服务 `enabled=false`：开机不自启、崩溃后不按退避自动重启；`svc enable/disable` 维护这个 sidecar，显式 `svc start` 仍然可用。
- **core 不接受 `capabilities`**：core 是生命周期单元，不参与 `tools/call`；定义里出现该字段即拒绝加载。能力面只属于 plugin。
- `<config>` 指平台配置目录（Linux `/etc/svcd`、macOS `/Library/Application Support/svcd/config`、Windows `%ProgramData%\svcd\config`）；`SVCD_CONFIG_DIR` 可覆盖，供测试与非 root 开发使用。
- `<logs>` 指平台日志目录（Linux `/var/log/svcd`、macOS `/Library/Logs/svcd`、Windows `%ProgramData%\svcd\logs`）；`SVCD_LOG_DIR` 可覆盖，供测试与非 root 开发使用。
- core 与 plugin 走同一监督器、同一 stdio NDJSON、同一依赖图、同一日志 / 状态；`svc list/status/start/stop/logs` 对两者一致；`install/uninstall/import/export/editconfig` 只对 plugin。

### 1.7 脚本服务（drop-in）

`<config>/services.d/` 里同时接受**不写 JSON 的脚本服务**：

- Unix：`<id>.sh`；Windows：`<id>.bat` / `<id>.cmd` / `<id>.ps1`。
- 文件名即服务 id（必须符合 `[a-z0-9][a-z0-9_-]{0,63}`）；`.disabled` sidecar 同样生效。
- 脚本第一个参数是 `start|stop|status`，同时注入 `SVCD_ACTION`；**每个动作都是可选的**，脚本声明哪个，svcd 才调用哪个。
- 声明方式：`.sh` 的 shell 函数（`start()` / `function start`）或 `case` 分支（`start)`）；`.ps1` 的 `function start`；`.bat` / `.cmd` 的 `:start` 标签（大小写不敏感）。注释里的同名不计。
- 未声明的动作由框架兜底：缺 `start` → `exec.args` 为空，svcd 从头执行脚本；缺 `stop` → 不跑 stop 钩子，直接进入普通停止序列（Unix SIGTERM；Windows 对没有停止钩子的 `exec` 脚本立即结束进程）；缺 `status` → `svc status` 不追加 probe。
- 已声明的 `start` 必须前台运行（可申请资源后 `exec` 真正的进程），不得 daemonize；`stop` 在 SIGTERM 前释放资源；`status` 是**细节补充**：stdout 作为 `details.probe`（截断到 8 KiB、多行以 `; ` 连接），退出码只追加到文本、不改变服务状态。
- 脚本派生的后台进程留在同一进程组（Unix）/ job object（Windows），停止时一起终止；组内任一进程存活时服务仍是 `Running`，整组退出才结束。
- svcd 把脚本合成为 core 服务：`exec.ready = Exec`，`lifecycle` 只为声明了的动作生成钩子；默认 `version = 1.0.0`，无 dependencies / capabilities / config。
- 同 id 的 JSON 定义优先；脚本跳过并告警。脚本必须非 symlink；Unix 还要求 exec 位；改脚本后重启 svcd 生效（v1 不做热加载）。
- `.ps1` 在 Windows 通过 `powershell -NoProfile -ExecutionPolicy Bypass -File <script> <action>` 调用。

```sh
#!/bin/sh
start()  { exec /usr/local/bin/real-service; }
stop()   { ...; }
status() { ...; }
action="${1:-$SVCD_ACTION}"
case "$action" in start|stop|status) "$action" ;; *) exit 64 ;; esac
```

上面的例子声明了全部三个动作；只写 `start` 时，`stop` 由框架终止进程、`status` 不产生 probe。`svc list/status/start/stop`、日志、退避、依赖图与 JSON core 相同；`svc status` 的 `details.probe` 来自 `status` 动作的 stdout。

### 1.8 脚本插件（core 脚本转 .spk）

core drop-in 脚本可以原样放进 `.spk` 变成 plugin：脚本不改，补 manifest。转换后执行路径与 §1.7 相同，但来源与信任换成签名包，并额外获得版本、热安装/卸载、config/export 与 Web UI。

- manifest 用 `schema: 2`，`exec.ready: "exec"`，按脚本声明写 `lifecycle.start/stop/status`（路径是包内相对路径，且必须在 `files[]`）。
- payload 不是 native 可执行文件（ELF / Mach-O / PE）时必须声明 `interpreter`：`{name, args?, version?}`。`name` 是 PATH 上的裸名；`version` 是数字约束，**为空/省略只校验存在**，不执行版本探测。Windows 上只解析 `.exe` / `.com`，不接受 `.cmd` / `.bat` shim。
- 安装时解析解释器到绝对路径并校验版本，失败不落盘；扫描时只重新解析路径。运行时统一 `interp + args + [script] + action_args`，`exec` 与 lifecycle 共用同一结果。Windows 没有 SIGTERM 的 polite 路径，`stop` 钩子结束后直接终止进程树。
- 脚本插件通常 `capabilities: []`，MCP 面只剩 `<id>.start`；会讲 MCP 的脚本可以 `ready: "notify"` 并声明能力。
- `plugin bundle --from-core --interpreter <name>` 会按脚本声明的 `start/stop/status` 自动生成 lifecycle；lifecycle 指向的其它脚本也要一起进 `files[]`。`hooks` / `config` / `ui` 仍按插件规则。
- `schema: 2` 是兼容门禁：老 daemon 在验包阶段拒绝，避免静默忽略 `lifecycle` / `interpreter`。

## 2. .spk 包格式

**.spk 只承载 plugin；core 服务不是 .spk（见 §1.6）。**

### 2.1 容器

- `.spk` = **ustar 子集，不压缩**；压缩交给传输层（store/CDN 的 `Content-Encoding`）。
- 条目顺序：`manifest.json` 第一、`signature.json` 第二，其余按路径排序；`mtime=0`、`uid/gid=0`、只允许普通文件与目录，可复现。
- 解包器拒绝：绝对路径、`..`、反斜杠、符号/硬链接、设备/稀疏文件、重复路径、清单外条目、setuid/setgid；总解压体积上限 512 MiB。
- 不用 zip（要 seek）、不用 tar.zst（核心里加解码器）。

### 2.2 manifest

JSON，`schema: 1` 或 `schema: 2`（2 只增加 `lifecycle` / `interpreter`，见 §1.8）。字段：

| 字段 | 类型 | 必需 | 语义 |
| --- | --- | --- | --- |
| `schema` | int | 是 | 1 或 2；2 增加 `lifecycle` / `interpreter`；未知主版本拒绝；老 daemon 拒绝 2 |
| `name` | string | 是 | 服务 id：`[a-z0-9][a-z0-9_-]{0,63}` |
| `version` | string | 是 | semver |
| `desc` | string | 是 | 一句话 |
| `platform` | string | 是 | 规范目标三元组，如 `x86_64-unknown-linux-musl`；设备与 `TARGET` 字符串精确相等，安装 + 加载双检 |
| `protocol` | int | 是 | 1；不匹配安装拒绝、加载再检 |
| `exec` | object | 是 | `{path, args?, env?, ready?}`；`ready` 默认 `notify`，可 `exec` |
| `interpreter` | object | 否 | schema 2：`{name, args?, version?}`；脚本 payload 必需，版本可空（只校验存在） |
| `lifecycle` | object | 否 | schema 2：`{start?, stop?, status?}`，路径包内相对且在 `files[]` |
| `dependencies` | [string] | 否 | 服务 id 列表，**无版本约束** |
| `capabilities` | array | 否 | `{name, desc, inputSchema}`；name 插件内唯一，`svc.` 禁用；只允许 `ready=notify` |
| `ui` | object | 否 | `{type: web / gui / headerless, entry?}`；`entry` 相对 `payload/web/`，默认 `index.html` |
| `files` | array | 是 | 除两个清单外的全部文件：`{path, size, sha256, mode}` |
| `hooks` | object | 否 | `{postInstall?, preRemove?}`，指向 `hooks/` 内可执行 |
| `config` | string | 否 | 配置模板在包内的相对路径，默认 `config/config`；必须在 `files[]` |
| `configCheck` | object | 否 | `{path, args?}`；校验命令，path 在 `files[]`、可执行；声明 check 却没有 config → 安装失败 |

- 文件名仍是 `<name>-<version>-<arch>.spk`，`arch = platform.split('-')[0]`。
- **canonical manifest**：打包器必须产出「排序键 + 紧凑 + 无尾换行 + 无 BOM」的 `manifest.json`；签名覆盖原样字节。
- 未知字段忽略；未知 `schema` 主版本拒绝；主版本内加法兼容。
- `arch` 不是合法字段；v1 不提供 `arch` 兼容层（见 §11）。

### 2.3 签名与信任

- 包内 `signature.json`：`{"alg":"ed25519","keyId":"...","sig":"base64"}`，签 `manifest.json` 的原样字节；`files[]` 钉每个文件的 sha256/size/mode。
- 验证顺序：读 manifest → 验签 → 校验 schema / platform / 路径 → 逐文件验 hash 后落盘；清单外条目直接拒；流式、逐文件可验。
- 信任根：`<config>/trust.d/<keyId>.pub`（**PEM SPKI**）；未知 keyId 拒绝；**不提供 `--insecure`**。
- 信任根只能由管理员显式添加：`svc trust list|add <file.pub>|remove <keyId>`，或 Web 控制台的 `POST /api/v1/trust?key_id=<id>` 上传；包内永不携带自己的信任根（自签自证没有意义）。
- 当前没有编译期 store 公钥，商城/发布接入时内置；`SVCD_TRUST_DIR` 可覆盖 trust.d，供测试与非 root 开发。
- 不签整个 tar、不做分离 `.sig`、不只靠商城 TLS。

### 2.4 安装布局与策略

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| data | `/var/lib/svcd` | `/Library/Application Support/svcd` | `%ProgramData%\svcd` |
| config | `/etc/svcd` | `/Library/Application Support/svcd/config` | `%ProgramData%\svcd\config` |
| logs | `/var/log/svcd` | `/Library/Logs/svcd` | `%ProgramData%\svcd\logs` |

- 版本目录 `<data>/plugins/<id>/<version>/` 保留包内布局；`<id>/current` 是版本号文本、`<id>/enabled` 是标记。
- **keep-old**（显式 `--strategy keep-old`）：新版本目录并存 → fsync → rename → 切 `current`；运行中的进程不热切，下次 start 用新版本；保留一份旧版本，`prune` 清非 current 且非运行中的版本。
- **replace**（默认，所有安装入口）：验签 + 校验清单（不落盘）→ 停服务 → 检查 plugin 目录空间：`free >= new` 则把旧版原地改名为 `.backup-<version>.<pid>` → 解包 → 写 `current` → 复制 config 模板；成功删备份，失败还原旧版与 `current`；`free < new` 但 `free + old >= new` 则删旧后装新（失败无回滚）；都不够则拒绝。空间按解包后体积计算。
- 空间不足自动降级为 `replace`；`install -` 只在 `replace` 下允许；降级需 `--allow-downgrade`；运行中的版本永不删；卸载默认保留 config/state，`--purge` 才清（并删除服务/安装钩子日志）。
- 所有安装入口默认 `replace`；`--strategy keep-old` 显式保留。Web 控制台不提供策略选择。

### 2.5 hooks

- 只接受 `hooks/` 下的可执行文件（Unix exec 位 + shebang，安装验包时校验；Windows `.exe` / `.cmd` / `.bat`，在执行 hook 时校验）；不解析内联命令。
- 环境清空后只给 `PATH`、`SVCD_PLUGIN_ID`、`SVCD_PLUGIN_VERSION`、`SVCD_ACTION`、`SVCD_DATA_DIR`、`SVCD_CONFIG_DIR`；Windows 额外给 `SystemRoot` / `WINDIR`（启动 `cmd.exe` 必需）；cwd = 版本目录；stdout/stderr 进 `<logs>/<id>-hooks.log`；60s 超时杀进程组。
- post-install 失败 → 报错；是否回滚由安装策略决定：replace 走既有回滚，keep-old 保留新旧两个版本（current 已切到新版本）。pre-remove 失败 → 记日志但继续卸载。
- 嵌入式构建（`--no-default-features`）默认拒绝带 hooks 的包，`plugin install --allow-hooks` / HTTP `allow_hooks=true` 放行；桌面构建（默认 features）默认放行。
- 桌面交互安装应先列出 hooks 再确认（`--yes` 跳过）：**尚未实现**——当前 CLI/HTTP 安装默认放行且没有交互确认步骤。
- **hooks 权限 = 安装者权限（root）**；信任只来自签名。
- **实现状态**：hook runner 已实现（`src/hook.rs`）：60s 超时、环境清空、Unix 进程组 / Windows 每 hook job 对象杀进程树、`<id>-hooks.log`；post-install 失败只返回错误，回滚完全由 keep-old / replace 策略处理；pre-remove 失败只记日志。

### 2.6 config（详见 §5.3）

- 模板在签名范围内（`files[]`），活动副本不在；活动副本在 `<data>/plugins/<id>/config`，root:svcd 0640。
- 安装时缺则复制模板、存在则不覆盖；升级保留；模板 hash 变化写 `config.new`；卸载默认保留。

## 3. IPC、MCP 与 HTTP UI

### 3.1 控制通道

- 一套 **JSON-RPC 2.0 + NDJSON** 信封跑三条链路：插件 stdio、`svc`/`mcp` 的 UDS / 命名管道、UI 的 `POST /plugin-ui/{id}/rpc`。这三条是控制面；HTTP 的 `/mcp` 与 `/api/v1` 是另外两个面（§3.4 / §3.5），不复用 NDJSON 分帧。
- `svc`/`svcd` 的本地管理通道用 UDS（Unix）/命名管道（Windows）上的 NDJSON；启动时生成 `<config>/svcd.token`（0640），请求携带该 token。Unix socket 0660；Windows 管道 ACL 仍是默认值，C8 的组授权细化随该边界实现补齐。
- 帧：UTF-8、LF、禁内嵌换行与 BOM；Windows 二进制模式；单帧上限 1 MiB，超限 = 协议违规，按崩溃路径杀进程。
- 流控：每插件在途 64、排队 128；超出 `-32000 busy`；单请求默认 30s，超时先发 `notifications/cancelled` 再回错；id 用 svcd 侧单调 u64。
- 插件 → svcd 只允许响应与通知，不允许发请求（v1 不做双向 RPC）；stdout 只走协议帧，stderr 是行式日志；stdout EOF = 退出。
- 方法集 v1：`svcd.ping`（必需，就绪探针）、`tools/call`（必需）、`svcd.status`（可选）、`svcd.shutdown`（可选通知；**BusyBox 上不提供**，见 §13-C7）、`notifications/cancelled`、`notifications/message`。
- `tools/list` 不进路由：能力来自 manifest，运行时自报会与注册表分叉。
- 版本：manifest `protocol: 1`；不匹配安装拒绝、加载再检；无握手；每条消息可带 `_meta` 做诊断，未知 `_meta` 忽略。
- Windows 稳定版无 UDS，桌面走命名管道；命名管道必须显式设 DACL（不要用默认 Everyone 读）。
- **实现状态（2026-11-12，#31）**：插件 stdio 的 NDJSON 分帧（1 MiB）、svcd 侧单调 id、每插件 64 在途、30s 超时先发 `notifications/cancelled` 再回工具错误、收帧超限杀进程、stdout EOF 按退出处理、插件 stdout 非协议帧按违规处理、崩溃摘能力并失败在途调用均已实现；svcd 暴露 `registry.tools` / `registry.call`（别名 `tools/list` / `tools/call`），能力描述与 `inputSchema` 从 manifest 透传。**本票未实现 §3.1 的 128 排队**：第 65 个并发调用直接回 `-32000 busy`（票面验收范围），排队留给后续票。

### 3.2 MCP 聚合

- 语义在 svcd，协议在 `mcp` 桥：svcd 提供 `registry.tools` / `registry.call` / `svc.*`，不认识 `_meta` / `resultType` / `ttlMs`；桥负责 era 判定、`_meta` 校验、缓存装饰、错误透传。
- 每个客户端一个桥进程；桥不拉起 svcd，连不上 stderr 报错 + exit 1；多个桥并发连同一个 svcd。
- dual-era：首个请求是 `initialize` → legacy；否则 modern（按 `_meta` 校验）。modern `2026-07-28`；legacy 支持 `2025-11-25` / `2025-06-18` / `2025-03-26`。
- modern：`server/discover`（MUST）；每请求校验 `_meta.protocolVersion` + `clientCapabilities`；result 带 `resultType:"complete"`；`tools/list` 带 `ttlMs:1000`、`cacheScope:"private"`，`listChanged:false`。
- legacy：本地回 `initialize`；`capabilities.tools.listChanged:true`；桥每 1s 轮询 `registry.version`，变了发 `notifications/tools/list_changed`。
- 错误映射：未知工具 / 参数不合法 / 缺 `_meta` → JSON-RPC error；已知但当前不可调用 → `isError:true` + 先 `<id>.start`；崩溃/超时/插件 error → `isError:true`；协议违规 → `isError:true` 并按崩溃处理；桥连不上 svcd → 在途调用 `isError:true` 后 exit 1。
- `inputSchema` 原样透传，聚合层**不做 JSON Schema 校验**（显式偏离，插件自己校验）；`svc.*` schema 由聚合层写死。
- 工具名 `<serviceId>.<cap>`，按名稳定排序；死 plugin 只出 `<id>.start`。
- `tools/list` / `tools/call` 只聚合 plugin 能力；core 不出现在工具面。`svc.*` 是管理工具，不是 core 的能力。
- **实现状态（2026-11-12，#32）**：`mcp` stdio 桥已实现 dual-era：首个请求 `initialize` → legacy，否则 modern；modern `server/discover`、每请求 `_meta` 校验（缺字段 `-32602`，版本不符 `-32022` + `data.supported`）、所有成功 result 带 `resultType:"complete"`、`tools/list` 带 `ttlMs:1000` / `cacheScope:"private"`、结果附 `_meta.serverInfo`；legacy `initialize` / `ping` / `notifications/initialized`、1s 轮询 `registry.version`（工具列表的稳定哈希）并在变化时发 `notifications/tools/list_changed`。桥不拉起 svcd，连不上 stderr + exit 1；每个客户端一个桥进程，多个桥并发连同一个 svcd。

### 3.3 HTTP UI

- `/plugin-ui/{id}/…` 映射包内 `payload/web/`；`/plugin-ui/{id}/rpc` 是 POST JSON-RPC，转给该插件的 stdio 通道——同一信封，没有第二套协议。
- `{id}` 必须是合法路径段；SPA fallback 只对 `Accept: text/html`；HTML 注入 `<base href="/plugin-ui/{id}/">`，资源必须相对路径；v1 不做响应体重写。
- MIME 按扩展名；ETag = mtime+size；`no-cache` + 304；v1 不做 Range。
- 实时通道（2026-11-13 起，#73）：
  - `GET /plugin-ui/{id}/ws`：RFC 6455 WebSocket，只收文本帧；客户端发 `tools/call`（与 `/rpc` 同一语义），插件 `notifications/*` 广播回同一连接；1 MiB 消息上限、ping/pong/close、无压缩/子协议。
  - `GET /plugin-ui/{id}/events`：SSE，`text/event-stream`；插件 `notifications/*` 以 `event: message` / `data: <json>` / `id: <seq>` 下发，15s 心跳；上行仍走 `POST /rpc`——SSE 单向 + POST 合起来才是双向。
  - 事件源是插件 stdout 的 `notifications/*`（`notifications/message` 为主）；按插件广播，每插件最多 64 个订阅者、有界队列，慢客户端断开，不阻塞插件 stdout。
  - 每插件保留 256 条 / 2 MiB 的事件环；SSE 重连带 `Last-Event-ID` 时先补发 `seq > last` 的事件，再进入实时流。请求的 id 已早于环窗口时先发 `event: reset` + `data: {"reason":"replay-unavailable"}`，UI 应调 `tools/call` 重新拉状态；无/非法 `Last-Event-ID` 只进实时流。
  - 两条流复用 `/plugin-ui` 鉴权与同源 `Origin` 检查：LAN 未登录 401、跨源 403；WS 的 cookie 调用必须带 `Origin`。
- 监听默认 loopback；LAN 需显式开启。**插件不自起 HTTP**；外连外部 WS/SSE 仍由插件进程自己负责，核心只做 UI 侧通道。
- **实现状态（2026-11-13，#73/#75）**：SSE/WS 两条 UI 流已接入。`GET /plugin-ui/{id}/events` 走 chunked `text/event-stream`，事件来自 `Slot::handle_notification` 发布的进程内 EventBus；每插件保留 256 条 / 2 MiB 事件环，`Last-Event-ID` 重连先补发缺失事件，越界先发 `event: reset`。`GET /plugin-ui/{id}/ws` 完成 RFC 6455 握手/掩码/帧编解码，客户端文本帧只接受 `tools/call`，响应与插件 `notifications/*` 走同一连接；1 MiB 上限、ping/pong/close、64 订阅者/插件。两条流复用 LAN cookie/Bearer 与同源 Origin；WS cookie 必须带 Origin。可运行示例：`samples/realtime/`（README 含 bundle/install/打开 UI 的命令）。测试覆盖 SHA-1 accept-key、帧编解码、EventBus 回放/淘汰、SSE/WS TCP 传输、SSE 重连和真实 daemon 集成。
- **实现状态（2026-11-12，#33）**：`GET /plugin-ui/{id}/…` 从 `<data>/plugins/<id>/<version>/payload/web/` 提供静态资源（MIME 按扩展名、`Cache-Control: no-cache`、ETag=mtime+size、`If-None-Match` → 304，`X-Content-Type-Options: nosniff`）；HTML 在 doctype 后注入 `<base href="/plugin-ui/{id}/">`；SPA fallback 只对 `Accept: text/html`，缺失的脚本/样式仍是 404；`{id}` 必须通过 `package::valid_name`，路径逐段解码后拒绝 `.`/`..`/反斜杠/NUL，并对 canonicalize 后的路径做根目录越界检查。`manifest.ui.type` 只接受 `web`/`gui`（`headerless` 或无 `ui` 即 404）。`POST /plugin-ui/{id}/rpc` 只桥 `tools/call`：`params.name` 带不带 `<id>.` 前缀均可，转发为 `registry.call` 并把页面 id 回显；`-32700/-32600/-32601/-32602` 在本地返回，插件未运行是 `result.isError:true`（同一信封）。LAN 开启时 `/plugin-ui/*` 要求会话（`svcd_session` cookie 或 Bearer；默认 loopback 无需鉴权；#4 的「静态资源公开」已被 #24/#38 的会话模型取代）；`/rpc` 校验同源 `Origin`，cookie 调用必须带 `Origin`，但不额外要求 `X-Svcd-Csrf`（同源 Origin + SameSite=Strict 承担 CSRF/DNS rebinding）；`/plugin-ui/{id}`（无斜杠）在鉴权后 308 到带斜杠形式。v1 不做 Range；WebSocket/SSE 见上方 #73/#75 实现状态。

### 3.4 HTTP MCP 入口

- `POST /mcp`：MCP **modern-only**（2026-07-28）Streamable HTTP；JSON 响应，**无 session、无 SSE**；`server/discover`、`tools/list`、`tools/call` 与 stdio 桥共用 `registry.tools` / `registry.call` 语义，不另造工具面。
- 按 2026-07-28 Streamable HTTP 的传输要求校验 `MCP-Protocol-Version` 等头；正文每请求校验 `_meta.protocolVersion` + `clientCapabilities`；result 带 `resultType:"complete"`；`tools/list` 带 `ttlMs:1000` / `cacheScope:"private"`；不做 `subscriptions/listen`。
- 认证：`Authorization: Bearer <lan-token>`；不接受 cookie（避免 CSRF 面）；401 不带 token，403 token 错。
- 监听：与 HTTP UI 同一个 listener；默认 loopback，LAN 需显式开启；单请求体上限 1 MiB。
- legacy 客户端走 `mcp` stdio 桥（§3.2）；HTTP 只做 modern。
- 工具可见性/错误映射与 §3.2 完全相同；死服务只出 `<id>.start`。
- **实现状态（2026-11-12，#40）**：`POST /mcp` 已实现 modern-only Streamable HTTP：单请求单 JSON 响应、无 session / 无 SSE，`GET`/`DELETE` 返回 405。逐请求校验 `MCP-Protocol-Version`（必须等于正文 `_meta` 版本）、`Mcp-Method`、`tools/call` 的 `Mcp-Name`（支持 `=?base64?…?=` 哨兵）：头缺失或头/正文值不一致返回 400 + `-32020`；正文缺 `_meta` 字段按核心规范 400 + `-32602`（与 stdio 同一条错误）；未知方法 404 + `-32601`；未实现版本 400 + `-32022`（带 `supported`）；`Origin` 必须同源（否则 403）。`Authorization: Bearer <lan-token>` 必需：缺 token 401（带 `WWW-Authenticate: Bearer`）、token 错 403，cookie 完全不参与；token 未生成时端点保持关闭，`svc lan token rotate` 可在不开 LAN 时生成 token。`server/discover` / `tools/list` / `tools/call` 走与 stdio 桥相同的 `mcp::serve_modern`（`resultType:"complete"`、`ttlMs:1000`、`cacheScope:"private"`、`<id>.start` 可见性、`isError` 映射）；正文上限 1 MiB（超限 413）；通知 POST 返回 202 空体（该修订未定义通知的头要求，故不做镜像校验）；控制通道不可达仍回 JSON-RPC（与 stdio 同映射，不引入 503）；不做 `subscriptions/listen`。

### 3.5 HTTP 管理 API

- 前缀 `/api/v1`，JSON only（上传/下载为二进制），无 CORS；**覆盖 `svc` 的全部管理命令**：
  - `GET /api/v1/services` → `{services:[summary + hasConfig + hasUI]}`（§5.2 的 summary；`hasConfig` / `hasUI` 是 HTTP 列表附加项，表示活动配置与可打开 Web UI 是否存在）
  - `GET /api/v1/services/{id}` → `{summary, details}`
  - `POST /api/v1/services/{id}/start` → 同步到 ready，返回 `{summary}`；超时 504
  - `POST /api/v1/services/{id}/stop` → 返回 `{summary}`
  - `POST /api/v1/services/{id}/enable` / `disable` → 切换开机自启，返回 `{summary}`
  - `GET /api/v1/version` → `{version, protocol, platform}`
  - `GET /` / `GET /index.html` → 内嵌 Web 控制台（静态 HTML；页面本身不需要认证，API 调用仍走同一鉴权）
  - `POST /api/v1/plugins`：body 为 `.spk` 原始字节（`application/octet-stream`），query `strategy` / `enable` / `allow_hooks`；流式写临时文件，512 MiB 上限；返回 `{id,version,state}`
  - `GET /api/v1/trust` → `{keys:[keyId], directory}`；`POST /api/v1/trust?key_id=<id>`：body 为 PEM SPKI 公钥，写入 `<config>/trust.d/<id>.pub`；`DELETE /api/v1/trust/{keyId}` 撤销
  - `DELETE /api/v1/plugins/{id}?purge=false`：卸载；`purge=true` 删除版本、配置、状态与 `<id>.log` / `<id>-hooks.log`（含轮转），Web 控制台使用 `purge=true`
  - `POST /api/v1/import?config_id=<id>&no_config=false&force=false`：body 为 `.spk` / `.spkb` / 裸配置，自动识别，语义同 CLI
  - `GET /api/v1/export/{id}/spk`：从已安装版本重建的可安装、已签名 `.spk`（`application/octet-stream`；Web 控制台“导出”使用它）
  - `GET /api/v1/export/{id}`：整包 `.spkb` 备份容器（`backup.json` + `plugin.spk` + 配置；供备份/导入）
  - `GET /api/v1/export/{id}/config`：单配置
  - `GET /api/v1/config/{id}`：活动配置原始字节（`text/plain`；无配置 404）
  - `PUT /api/v1/config/{id}`：body 为候选配置；跑 `configCheck`，原子替换，保留 last-good；校验失败 409
  - `POST /api/v1/config/{id}/reset`：模板覆盖
  - `GET /api/v1/logs/{id}?lines=200`：返回文本尾部；`lines` 上限 2000；不提供 follow / SSE
  - `GET /api/v1/lan`：`{enabled,bind,tokenSet}`
  - `POST /api/v1/lan/enable`（body `{bind}`）/ `POST /api/v1/lan/disable` / `POST /api/v1/lan/token/rotate`（返回新 token 一次）
- `plugins` / `export` / `config` / `import` 端点只对 plugin；core 服务不能经 HTTP 安装、卸载、导出或编辑配置；`GET /services` 两者都返回。
- 认证：`Authorization: Bearer <lan-token>`，或登录时下发的 `svcd_api` cookie（Path=`/api/v1/`）；cookie 的写请求必须带同源 `Origin` + `X-Svcd-Csrf: 1`。
- **LAN token 等价于 root / Administrator**：install、uninstall、import、editconfig 都通过它授权。默认 LAN 关闭、无 TLS；只用于可信网络（见 §6 / §13-C5）。
- 语义映射 `svc.*`，与 CLI 同源；不写第二套状态。错误：`{error:{code,message}}`，400 / 401 / 403 / 404 / 409 / 413 / 500；start/stop 幂等。
- 上传大小：`.spk` / `.spkb` 512 MiB（与解包上限一致）；配置 1 MiB；日志响应 1 MiB。
- HTTP/1.1 keep-alive：同一连接顺序处理多个请求；`Connection: close` 显式关闭；不支持 pipelining；每个连接仍占一个线程。
- **嵌入式构建（#86）**：默认 feature `http`（HTTP 管理 API、内嵌控制台、插件 Web UI、LAN、MCP HTTP）。`cargo build --release --no-default-features` 移除这些，保留 daemon/supervisor、channel、CLI 与 MCP stdio；Windows 二进制约 1.14 MB → 1.00 MB，musl 约 1.36 MB → 1.21 MB。HTTP/channel/plugin reader/WS worker 线程使用 128–256 KB 显式栈（默认约 2 MB）；musl 下 daemon VM @30 插件由约 128 MB 降到约 11.6 MB，RSS 基本不变。
- **实现状态（2026-11-13，#79/#83）**：HTTP 监听改为阻塞 accept（rebind 用 loopback 探针唤醒并丢弃探针）；daemon 改用 condvar 唤醒控制通道和插件 stdout，事件循环只在 50ms 兜底等待中检查 shutdown/定时器，无 1ms 轮询；`/api/v1` 与 `/mcp` 支持 HTTP/1.1 keep-alive（顺序多请求，`Connection: close` 可关闭），WS/SSE 仍独占连接。性能对比见 `docs/research/2026-11-13-core-compat-and-performance.md`。
- **实现状态（2026-11-12，#37 / #38 / #39）**：已实现 `GET /services`、`GET /services/{id}`、`POST start/stop`、`GET /version`、`POST /plugins`、`DELETE /plugins/{id}`；默认绑定 `127.0.0.1:8787`（`SVCD_HTTP_PORT` 覆盖，`0` 为临时端口并写 `<config>/http.port`），**无 TLS**；`svc lan enable` 可切到 LAN 并要求 Bearer/cookie 认证（#38）。#39 补齐 `GET/PUT /config/{id}`、`POST /config/{id}/reset`、`GET /export/{id}/spk`（可安装 `.spk`）、`GET /export/{id}`（`.spkb` 备份）、`GET /export/{id}/config`、`POST /import`（自动识别）、`GET /logs/{id}?lines=N`（默认 200、上限 2000、不提供 follow）、`POST /services/{id}/enable|disable`；这些端点只接受 plugin id，core 服务返回 404，方法不符返回 405。`GET /` 提供内嵌控制台（插件列表/安装/启停/自启/打开/配置/日志/导出）：上传按钮直接弹文件框，公钥上传在顶栏，无配置的插件不显示“配置”，有 web/gui UI 的插件显示“打开”，日志弹框可 3 秒自动刷新，删除走 `purge=true`。“安装包”导出 `/export/{id}/spk`（签名包，使用包内配置模板）；“导出备份”导出 `/export/{id}` 的 `.spkb`（签名包 + 当前活动配置，供 `plugin import` 恢复）。页面无需认证即可加载，API 调用仍走同一鉴权。

### 3.6 LAN 鉴权与会话

- 默认只绑 `127.0.0.1`；`svc lan enable --bind 0.0.0.0` 才开 LAN。
- 首次开启生成 32 字节随机管理员 token（**等价 root / Administrator**），打印一次并写 `<config>/lan-token` 0600；`svc lan token rotate` 轮换；无用户体系、无注册。
- 登录：`POST /login` 带 token → 设两个 cookie：`svcd_session`（Path=`/plugin-ui/`）与 `svcd_api`（Path=`/api/v1/`），均 HttpOnly、SameSite=Strict、Max-Age=86400、无 Secure（无 TLS）；滑动续期 24h；`POST /logout` 清两个。同一 token 也可直接作为 `Authorization: Bearer` 访问 `/mcp` 与 `/api/v1/*`。
- LAN 开启时 `/plugin-ui/*` 与 `/rpc` 都要求会话（默认 loopback 无需鉴权）；`/plugin-ui/{id}`（无斜杠）在鉴权后 308 到带斜杠形式，保证 path-scoped cookie 生效；`/rpc` 保留 Origin 校验：只接受同源 Origin/Host、拒绝 CORS，cookie 调用必须带同源 `Origin`，但不要求 `X-Svcd-Csrf`（SameSite=Strict + 同源 Origin 承担 CSRF/DNS rebinding）；`/api/v1` 的 cookie 写仍要求 `X-Svcd-Csrf`。
- 登录失败固定 1s 延迟，连续 10 次锁 5 分钟（内存）。
- **v1 不支持 TLS**：核心不终止、不生成证书；要 HTTPS 就在前面放反代。明文 token 过 LAN 的风险写进文档；默认关闭 LAN 是安全默认。
- 单用户 / 单角色；多用户、角色不做；HTTP 管理 API 见 §3.5。
- **实现状态（2026-11-12，#38）**：默认 loopback；`svc lan enable [--bind]` 生成 32 字节 token（0600，只打印一次）、切 listener 绑定；`POST /login` 下发 `svcd_session`/`svcd_api` cookie；`/api/v1` 收 Bearer 或 cookie，cookie 写要求同源 `Origin` + `X-Svcd-Csrf: 1`；登录失败 1s、10 次锁 5 分钟；`svc lan disable` / `token rotate`。`/mcp` 的 Bearer 面由 #40 实现（见 §3.4）；cookie 不参与 MCP 认证。

## 4. OSAL 与平台边界

**OSAL = 5 个边界 + 0 个 trait。**

| 边界 | 形状 | 平台差异 | 形态 |
| --- | --- | --- | --- |
| `proc` | `spawn / request_stop(polite / force) / wait_exit / liveness` | fork+subreaper vs CreateProcess+Job | cfg 模块 |
| `wait` | `wait(&[Handle], timeout) -> Ready` | poll(2) vs IOCP/线程 | driver |
| `channel` | `bind(Channel) -> Listener -> Read+Write` | UDS vs 命名管道 + DACL | cfg 工厂 |
| `install` | `plan(os) -> InstallPlan { files, commands }` | 有，但是数据 | 纯数据 |
| `webview` | webview/webview 薄包装 | 无 | 桌面独占 |

- 零 trait：同一构建只有一份实现、差异是一次性 syscall 映射，trait 没有运行时收益；可测性靠「纯决策核」（`Action` 枚举 + 派发），不靠 fake。
- Unix 与嵌入式共用一条同步循环（单线程，`wait` 唯一 OS 触点，timer 线性扫描，不引 mio/tokio）；Windows 桌面走独立 driver。
- Windows 两条事实外溢：`WaitForMultipleObjects` 64 句柄上限（每插件 3 个 ≈ 20 插件封顶）；管道没有「可读」可等待态，数据就绪靠 overlapped I/O 或线程。
- 事件只认 `ShutdownRequested / ChildExited / Inbound`；Unix 用 self-pipe（不用 signalfd），Windows 用控制处理器线程 → event。
- 优雅关闭策略平台无关：依赖逆序 → polite → 等 T → force；Windows 的 polite 降级为「控制帧 + 等 T + TerminateProcess」，写进规格，不被抽象抹平。
- `status` 只作 manifest 可选能力、不进 OSAL；`[auto]` 由 svcd 推断，不由服务自报。

## 5. CLI、status、config、logs

### 5.1 入口

- `svcd`：守护进程；`svcd run --foreground`、`svcd install-service / uninstall-service`、`svcd --install [--conf <file>] / --uninstall`。
- `svc`：服务 CLI，接受任意服务 id（core 或 plugin）：`list / status / start / stop / enable / disable / logs / lan / version`。
- `plugin`：插件 CLI，**只接受 `kind=plugin` 的 id**；动词与 `svc` 同源，另加 `install / uninstall / editconfig / export / import`。传 core 服务 id 报错「不是插件」。
- `mcp`：stdio MCP 桥，每个客户端一个进程。
- `svc ui`：桌面 GUI 入口，客户端；关窗不退 svcd。

### 5.2 status

- `summary`：id、kind、state、enabled、pid、since、version；HTTP `GET /api/v1/services` 的投影额外带 `hasConfig`（活动配置是否存在）与 `hasUI`（manifest 是否声明可打开的 web/gui UI），`svc list` 的 payload 不变。
- `details`：dependencies[]、last_exit、next_at、ready_since、failures、config、probe（仅 `svc status` 探测 core 的 status 动作时出现，内容为该动作的 stdout/摘要）。
- `svc list` 列：`ID  KIND  STATE  AUTO  PID  VERSION`；AUTO 显示 `[auto]` 或 `-`。
- `svc list` 只投影受管服务（core + plugin），不含内置 `svc` 自身（svcd 由系统服务管理器拉起，不归自己管）；`svc status svc` 仍可读。
- `svc status <id>`：逐字段块；`--json` 原样输出 `{summary, details}`。
- `[auto]` 只映射 `enabled`（开机启动）；崩溃重启策略放 details 的 `restart`（如 `on-failure`），不用 `[auto]`。
- 退出码：0 成功；1 服务不存在；2 守护进程不可达；64 参数错误。
- MCP 的 `svc.list` / `svc.describe` 用同一 payload，不另造字段。

### 5.3 config

- **只适用于 plugin**：core 的配置由脚本/bin 自己管，svcd 不提供 `config` / `editconfig` / `export`。
- 模板：`config` 字段，单文件、包内相对路径、默认 `config/config`、必须在 `files[]`；禁止绝对路径 / `..` / 符号链接 / 目录；显式路径缺失 = 安装失败；缺省且默认文件不存在 = 无配置。
- 活动副本：`<data>/plugins/<id>/config`，root:svcd 0640（无组 0600）；安装时缺则复制模板、存在则不覆盖；记录模板 sha256 到 `config.template.sha256`。
- 启动不覆盖、不自动重建；缺失就按无配置启动，不注入 `SVCD_CONFIG`；有配置时 svcd 注入 `SVCD_CONFIG` 给插件。
- 升级保留活动副本；模板 hash 变化写 `config.new`（0640）并警告；`--reset` 用模板覆盖（先备份 last-good）。
- `editconfig <id>`：要 root；flock `<id>/config.lock`；临时文件 → `$EDITOR`（fallback `vi`）→ 可选 `configCheck`（10s，候选路径，`SVCD_CONFIG`）→ fsync + 原子 rename，旧文件存 `config.last-good`；支持 `--print` / `--file` / `--reset`；不自动重启，打印「restart to apply」。
- 导出/导入：
  - `export --config-only`：原样活动副本，默认 `<id>-<version>.config`，0600。
  - `export`：`.spkb` 备份容器（ustar）：`backup.json`（首条目）`{schema:1,id,version,exportedAt,configSha256}` + `plugin.spk`（从已安装版本重建的签名包）+ `config`；容器不签名，导入验内层签名。
  - `import <file>`：tar 首条目 `backup.json` → `.spkb`；`manifest.json` → `.spk`；其它 → 裸配置，必须 `--config <id>`。id 不符拒绝；版本不同警告；`--no-config` 跳过；`--force` 只跳过同版本检查，不跳签名。
- 配置上限 1 MiB；插件私有格式（JSON/TOML/YAML/ini 都行），svcd 不解析、不做 schema、不做升级字段合并；无 `configCheck` 时只保留 last-good。
- 权限：写（install / editconfig / import / uninstall）要 root；只读（status / logs / `editconfig --print` / `export`）组可用；配置明文，不加密（见 §13-C5）。
- **实现状态（2026-11-12，#34 / #39）**：安装复制模板 / `config.template.sha256` / 模板变化写 `config.new`；`editconfig`（`--print/--file/--reset`、`$EDITOR`、`configCheck` 10s、last-good、flock）；`export`（config-only / `.spkb`）与 `import`（`.spkb` / `.spk` / 裸配置自动识别）已实现。HTTP config/export/import 端点已由 #39 实现（`GET/PUT /config/{id}`、`POST /config/{id}/reset`、`GET /export/{id}` 与 `/export/{id}/config`、`POST /import`）；hooks 由 hook runner 执行（post-install 失败回滚，pre-remove 失败不阻塞卸载）。

### 5.4 logs

- 落点：`<logs>/<id>.log`；svcd 自身 `svcd.log`；hooks `<id>-hooks.log`。
- 轮转：1 MiB × 3（`.log`、`.log.1`、`.log.2`），丢最旧，不压缩，不引 logrotate。
- 权限：0640 root:svcd；`svcd` 组只读；插件 stdout/stderr 由 svcd 捕获后写，插件不能改。
- `svc logs <id> [--follow] [--lines N]`；组可读；不做远程 syslog、不做 JSON lines。
- 嵌入式同一策略，`/var` 落 overlay，1 MiB × 3 控制 flash 写放大；掉电丢当前缓冲可接受。
- 不做日志汇聚、远程推送、journald / Event Log 双写。

## 6. 安全与权限

- **所有服务进程（core 脚本/bin、plugin）与 hooks 以 svcd 身份运行**：Linux / macOS = root，Windows = LocalSystem。不降权、不做每服务身份、不做沙箱、不设 `privilege` 字段。
- 服务之间没有身份隔离：可互读数据、互发信号；这是接受的取舍。core 定义无签名，信任来自 root 所有的 `services.d/*.json` 与本机管理员；plugin 信任来自 Ed25519 签名 + `trust.d`。
- 本地授权两档：
  - root / Administrator：install、upgrade、uninstall、`editconfig`、`import`、LAN enable / token rotate。
  - `svcd` 组：start / stop / status / logs、`editconfig --print`、`export`（只读）。
- LAN 是另一套：管理员 token；UI 用 `svcd_session`，`/api/v1` 用 `svcd_api` 或 Bearer，`/mcp` 只收 Bearer；不映射 OS 用户。**HTTP 管理 API 覆盖全部 `svc` 命令，token 等价于 root / Administrator**；默认 LAN 关闭、无 TLS。
- 配置明文 0640 root:svcd；不接 Keychain / DPAPI / Secret Service / TPM、不加密；导出/备份含明文 secret，文档警告；插件自管密钥。
- **明确接受：任一服务/插件有 bug 或被攻破 = 整机 root / System**；补偿只在来源侧（core 的 root 文件权限、plugin 的签名/审核/`trust.d`）。

## 7. 构建、体积与 CI

- 语言：Rust 单语言；release profile：`opt-level=z` + fat LTO + `codegen-units=1` + `panic=abort` + strip（#16 骨架）。
- 目标：
  - 嵌入式：`x86_64` / `aarch64` / `armv7` + musl；
  - Windows：`x86_64-pc-windows-msvc`；
  - macOS：`aarch64-apple-darwin`、`x86_64-apple-darwin`（分包）；
  - Linux 桌面：`x86_64-unknown-linux-gnu`（webkit2gtk 只能动态链，**musl 不可行**）。
- 工具链：ubuntu-24.04 上用 cargo-zigbuild（pin `=0.24.1`）+ Zig（`0.17.1`）出三架构 musl；Windows/macOS 原生 runner；唯一高风险环是 armv7+musl 的 zig 链接。
- 计量：multicall 单二进制，strip 后、未压缩、十进制字节；硬链接算一次；编译进二进制的资源算；安装的 unit/config 与插件不算。
- 门禁：嵌入式 **≤ 2,000,000 B**、桌面 **≤ 30,000,000 B** 超限 fail；基线 +2% warning；`tools/measure_size.py` + `.github/workflows/size.yml` + `ci/size-baseline.json`。
- 实测基线（2026-11-12 本机）：musl x86_64 `1,119,152 B`；Windows x86_64 `910,848 B`；macOS / Linux gnu 待 CI 首跑。
- **30MB 只约束核心二进制**；安装包不计门禁，只记录体积（见 §13-C2）。

## 8. 桌面交付

| 平台 | 安装包 | 最低系统 | 架构 | WebView | 签名 |
| --- | --- | --- | --- | --- | --- |
| Windows | MSI（WiX + Burn） | Windows 11 build 22000+（测试 25H2 / 26H1 / 26H2） | x86_64（ARM64 后补） | Evergreen WebView2；缺失跑 bootstrapper（≈2MB） | Authenticode（硬件/云 HSM） |
| macOS | flat pkg（可套 dmg） | macOS 15 | arm64、x86_64 分包 | 系统 WKWebView | Developer ID + 公证 + staple |
| Linux | deb + rpm | Ubuntu 22.04（universe）/ Debian 12 / Fedora 43+ / Arch | x86_64 先出（aarch64 后补） | libwebkit2gtk-4.1-0（声明依赖） | 仓库 GPG |

- 系统级守护进程 + 专用组 `svcd`：安装器建组并把安装用户加入（Linux postinst 用 SUDO_USER、macOS pkg 取 console user、Windows MSI custom action 用 LogonUser；取不到则提示手动加组）。
- GUI 入口 `svc ui`：客户端；安装器建快捷方式 / `.desktop` / `.app` 包装；关窗不退 daemon。
- macOS：只有 pkg 能以 root 跑脚本装 LaunchDaemon；不用 SMAppService（绑 .app、CLI/无人值守更绕）；Info.plist `NSAllowsLocalNetworking`；spawn 子进程不需要额外 entitlement；插件是独立进程，不 dlopen 第三方 dylib；Apple Developer $99/年。
- Windows：MSI 每机器安装（ALLUSERS=1 + ProgramFiles64Folder），WiX `ServiceInstall`/`ServiceControl` 注册 SCM；Burn bundle 带 WebView2 bootstrapper；不选 MSIX（包身份/按用户模型不贴每机器守护进程）、不选 NSIS（无 MSI 修复/卸载策略）；Win10 已 EOL，不写进支持矩阵；SmartScreen 是信誉制。
- Linux：webkit2gtk-4.1 基线；放弃 RHEL 9（只有 4.0）；只出 deb + rpm，不出 AppImage / Flatpak / Snap；桌面 Linux 必须 gnu target；自建 apt 仓库必须签 Release/InRelease，rpm 仓库 `gpgcheck=1`。
- 发布前置：Apple Developer 账号、Windows 代码签名证书 + 硬件/云 HSM、CI 的 macOS 签名/公证与 Windows 签名 job、Linux 打包与仓库签名。

## 9. 嵌入式目标

- 版本：**当前 stable + 上一 stable** 两条 OpenWrt 线；发布时在 `targets.toml` pin 具体版本，EOL 即移出；不承诺更老版本。
- 架构：x86_64、aarch64、armv7（与 CI musl 三架构一致）；mips 不承诺。
- libc/内核：musl；kernel ≥ 5.15；procd 为默认，BusyBox init 保留 best-effort（五份安装计划）。
- 存储：overlayfs；`/var` 落 overlay（持久），不用 tmpfs 存日志/配置；安装后至少留 8 MiB 给插件 + 日志 + 配置。
- 包管理：按目标版本实际用 opkg 或 apk；svcd 自身打 ipk / apk 包。
- 功能面：UI 与 `mcp` 保留；TLS 不链；不编 WebView。
- CI：musl x86_64/aarch64/armv7 + 2,000,000 B 门禁；`targets.toml` 驱动。
- 非目标：mips、非 Linux 嵌入式、OpenWrt 之外的 BusyBox 系统（best-effort，不承诺）。

## 10. 自升级

- **v1 不做 svcd 自升级**；升级交给平台包管理器（apt/dnf、MSI、pkg、opkg/apk）。
- 不内置更新器、不联网检查版本、不自动替换二进制；`svc version` 只报版本。
- 签名沿用包签名（Authenticode / 公证 / deb 签名 / 仓库 GPG），不做第二套 Ed25519 自更新。
- 服务重启由包管理器脚本负责；插件进程不随 svcd 重启，按 §2.4 下次 start 生效。
- 回滚 = 包管理器装回旧版本；不维护 A/B 分区。
- 兼容：svcd 升级后按 `protocol` / `platform` 校验旧插件；不兼容的拒绝 start 并提示升级插件。

## 11. 兼容与迁移

- `platform` 精确匹配；Rosetta 与 Windows ARM64 的 x64 模拟留作后续放宽，不默认开。
- `protocol: 1` 精确匹配；主版本内加法兼容；未知方法 `-32601`，未知字段忽略。
- `config` / `configCheck` 为加法字段；旧 svcd 忽略它们（版本偏斜，文档写明）。
- `arch` 不是合法字段：v1 的 `schema: 1` 已重写为只含 `platform`；不提供 arch 兼容层。
- `schema: 2` 只增加 `lifecycle` / `interpreter`；老 daemon 在验包阶段拒绝 2，避免静默忽略导致脚本以错误参数启动。schema 1 的旧包行为不变。
- 插件升级：默认 `replace` 采用 §2.4 的「空间检查 → 备份/还原或删旧」语义；显式 `--strategy keep-old` 走原子并存；活动配置在版本目录外，先删旧版本不会丢。

## 12. 票与笔记索引

| 票 | 标题 | 结论一句话 |
| --- | --- | --- |
| [#2](https://github.com/webxxxxx/plugin-manager/issues/2) | 插件 IPC 与反向代理栈 | NDJSON 一套分帧跨 stdio/UDS/命名管道；Windows 稳定版无 UDS；musl 空壳 364.7KiB、axum 748.8KiB |
| [#3](https://github.com/webxxxxx/plugin-manager/issues/3) | 进程监督内核与四平台服务管理器对接 | s6 不可复用；自研抄 run/finish 与二分支退避，以 BusyBox 为基线 |
| [#4](https://github.com/webxxxxx/plugin-manager/issues/4) | 插件 IPC 协议形态 | JSON-RPC 2.0 NDJSON；1MiB/64/30s；stdio 默认；无握手 |
| [#5](https://github.com/webxxxxx/plugin-manager/issues/5) | MCP 协议现状与内嵌聚合 Server | 规范 2026-07-28；聚合归聚合方；自研最小 stdio 层 |
| [#6](https://github.com/webxxxxx/plugin-manager/issues/6) | MCP 聚合层对外形态 | 语义在 svcd、协议在桥；dual-era；ttl/list_changed；isError |
| [#7](https://github.com/webxxxxx/plugin-manager/issues/7) | Rust 体积账本 | 单二进制成立但很窄；TLS 必须外置；Go 出局 |
| [#8](https://github.com/webxxxxx/plugin-manager/issues/8) | 桌面 WebView 方案 | webview/webview；系统引擎；嵌入式不编译即零残留 |
| [#9](https://github.com/webxxxxx/plugin-manager/issues/9) | 服务模型与 IPC 端点 | PM 退役；core=脚本/bin，plugin=.spk；svc list 与 tools/list 同源 |
| [#10](https://github.com/webxxxxx/plugin-manager/issues/10) | CI 交叉编译与体积测量 | cargo-zigbuild 0.24.1 + Zig 0.17.1；strip 后字节；2% 告警 |
| [#11](https://github.com/webxxxxx/plugin-manager/issues/11) | 体积预算分配 | 嵌入式放宽到 2MB；单二进制 + 三硬链接；桌面仅 webview feature |
| [#12](https://github.com/webxxxxx/plugin-manager/issues/12) | WireGuard .spk 样张 | 格式成立；12,800 B；协议跑通；平台标识缺口转 #18 |
| [#13](https://github.com/webxxxxx/plugin-manager/issues/13) | .spk 包格式与安装布局 | 严格 tar；Ed25519 签 manifest；keep-old/replace；hooks 60s |
| [#14](https://github.com/webxxxxx/plugin-manager/issues/14) | 监督器实现 | 6 态纯核；退避 1/2/4/8/16/30s；probe-on-spawn；五份安装计划 |
| [#15](https://github.com/webxxxxx/plugin-manager/issues/15) | OSAL 抽象清单 | 5 个边界 + 0 trait；Unix/嵌入式同步循环，Windows 独立 driver |
| [#16](https://github.com/webxxxxx/plugin-manager/issues/16) | CI 尺寸探针 | measure_size.py + size.yml；2MB/30MB fail；实测 504,544 / 327,168 B |
| [#17](https://github.com/webxxxxx/plugin-manager/issues/17) | 桌面构建裁剪与交付 | 系统级 daemon；pkg/MSI/deb+rpm；30MB 只量二进制；三平台签名 |
| [#18](https://github.com/webxxxxx/plugin-manager/issues/18) | manifest 平台标识 | arch → platform 三元组；安装+加载双检；arch 只派生 |
| [#19](https://github.com/webxxxxx/plugin-manager/issues/19) | 插件身份与权限边界 | 全部 root/System；不降权、不设 privilege；装/升/卸要 root |
| [#20](https://github.com/webxxxxx/plugin-manager/issues/20) | config 字段与配置流 | config/config 模板；活动副本 0640；editconfig 要 root；.spkb |
| [#21](https://github.com/webxxxxx/plugin-manager/issues/21) | 依赖版本约束 | v1 不做版本约束；id 列表；start 按拓扑拉起，缺则拒绝 |
| [#22](https://github.com/webxxxxx/plugin-manager/issues/22) | svc status 输出 | summary+details；[auto]=enabled；退出码 0/1/2/64 |
| [#23](https://github.com/webxxxxx/plugin-manager/issues/23) | 日志落点与轮转 | per-service 文件；1MiB×3；不接平台日志；组只读 |
| [#24](https://github.com/webxxxxx/plugin-manager/issues/24) | LAN 鉴权与会话 | 默认 loopback；管理员 token；UI/API cookie；MCP Bearer；无 TLS；单用户 |
| [#25](https://github.com/webxxxxx/plugin-manager/issues/25) | svcd 自升级 | v1 不做；交给平台包管理器；不维护 A/B |
| [#26](https://github.com/webxxxxx/plugin-manager/issues/26) | 嵌入式目标平台 | OpenWrt stable 两条线；三架构 musl；kernel≥5.15；overlay |
| [#27](https://github.com/webxxxxx/plugin-manager/issues/27) | 配置敏感值保护 | 明文 0640；不做 keyring/加密；插件自管密钥 |

研究笔记：

- [插件 IPC 与反向代理栈](docs/research/2026-11-12-plugin-ipc-and-reverse-proxy.md)
- [MCP SDK 与聚合](docs/research/2026-11-12-mcp-sdk-and-aggregation.md)
- [进程监督与四平台服务管理器](docs/research/2026-11-12-process-supervision-and-service-managers.md)
- [嵌入式 1MB 运行时可行性](docs/research/2026-11-12-embedded-1mb-runtime-feasibility.md)
- [CI 交叉编译与体积测量](docs/research/2026-11-12-ci-cross-compile-and-size-measurement.md)
- [桌面 WebView 方案](docs/research/2026-11-12-desktop-webview-options.md)
- [桌面发行基线](docs/research/2026-11-12-desktop-packaging-and-platform-baselines.md)
- [插件降权与隔离机制](docs/research/2026-11-12-plugin-privilege-mechanisms.md)
- 其他：[GLOSSARY.md](GLOSSARY.md)、[ADR 0001](docs/adr/0001-single-multicall-supervisor.md)

## 13. 冲突、缺口与待决

> 这些不是新的产品取舍，是 26 张票合到一起后暴露的**不一致或没覆盖到的地方**。按优先级处理。

| # | 类型 | 问题 | 证据 | 建议 |
| --- | --- | --- | --- | --- |
| C1 | **冲突（已定）** | manifest `schema: 1` 仍是 #13 写的、含 `arch`；#18 把 `arch` 换成 `platform`，属破坏性字段替换，但没有 bump schema 版本。旧解析器读新包会因缺 `arch` 失败，新解析器读旧包会因缺 `platform` 失败 | #13 Answer / 评论、#18 Answer | **已定（2026-11-12）**：删除 `arch`，只留 `platform`；`schema: 1` 重写，不提供 arch 兼容层（尚无外部稳定包；若已发过再 bump 2）。 |
| C2 | **缺口（推迟）** | CI 只构建 3 个 musl + Windows，nightly 只加 `aarch64-apple-darwin`；#17 要求 Linux 桌面 `x86_64-unknown-linux-gnu`、macOS `x86_64-apple-darwin`，且没有 MSI / pkg / deb / rpm 的打包 job。桌面二进制与安装包都没被门禁覆盖 | #16 `ci/README.md`、#17 Answer | **推迟（2026-11-12）**：本轮不处理；做打包流水线时再加 gnu x86_64 / macOS x86_64 与各平台打包 job；安装包体积先记录、不门禁。 |
| C3 | **待决（已定）** | MCP 原先只有本地 stdio 桥；#11 留了「要的话单开票」，之后没有票；#24 的 LAN 鉴权只覆盖 UI / `/rpc`，没有 `POST /mcp` | #11 Answer、#24 Answer、#6 | **已定（2026-11-12）**：HTTP 增加 `POST /mcp`，modern-only Streamable HTTP、JSON、无 session/SSE、Bearer；legacy 走 stdio 桥（§3.4）。 |
| C4 | **缺口（已定）** | HTTP 面原先只有插件 UI 的 `/rpc`；没有 HTTP 管理 API（list/start/stop）。嵌入式浏览器用户无法管理插件，只能 SSH/CLI | #4 UI 通道、#24 Answer | **已定（2026-11-12）**：HTTP 增加 `/api/v1` 管理 API，覆盖全部 `svc` 命令（services / install / import / export / config / logs / lan / version）；Bearer 或 cookie，写操作 CSRF；token 等价 root，默认 LAN 关闭、无 TLS（§3.5 / §6）。 |
| C5 | **边界（已定）** | TLS 不在核心；#24 的 token 明文过 LAN；#17 的 macOS ATS 只是本地回环例外。这是已接受的取舍，不是 bug | #11、#24、#17 | **已定（2026-11-12）**：v1 不支持 TLS，不生成证书；HTTPS 靠前置反代；LAN 只用于可信网络；token 等价 root，默认 LAN 关闭。不要在核心里偷偷加 TLS。 |
| C6 | **已澄清** | 组权限：#17 说组管生命周期；#19 说组只做 start/stop/status/logs；#20 又给了只读 `export` / `editconfig --print` | #17、#19、#20 | 以 §6 的授权矩阵为准：组只读 + 生命周期；写操作 root；#19 已补评论 |
| C7 | **平台例外** | #4 方法集有 `svcd.shutdown`；#14 说 BusyBox 上不提供 `svcd shutdown` | #4、#14 | 规格保留方法，但注明 BusyBox 不实现；插件必须能接受「不发 shutdown 直接 SIGTERM」 |
| C8 | **实现约束** | Windows 命名管道默认 DACL 给 Everyone 读；#17/#19 要求组授权，不能吃默认值 | #15、#2 笔记 | 显式设置管道 DACL（SYSTEM/Administrators 完全，`svcd` 组读写）；不依赖默认 |
| C9 | **边界（已澄清）** | 30MB 只量核心二进制，安装包不计；#11/#16 的预算表容易让人以为含安装包 | #11、#16、#17 | 文档统一写「核心二进制」，安装包体积单列记录 |
| C10 | **文档债** | #13 的正文/Answer 仍写 `arch`；#18 只在评论里回填；样张已同步但 schema 文档未重生 | #13、#18、#12 | 以本规格 §2.2 为准，重新生成 manifest schema 文档与样张说明 |
| C11 | **缺口（中）** | `svc ui` 只定了「入口」，没定界面内容；桌面商城客户端在 Out of scope，核心 UI 显示什么没写 | #17、地图 Out of scope | 明确 v1 `svc ui` 只做「本地服务列表 + 打开插件 UI」；商城另立 |
| C12 | **待定（低）** | OpenWrt 的「stable 两条线」还没 pin 具体版本；`targets.toml` 未落地 | #26 | 发布前 pin 版本并进 CI；EOL 即移出 |
| C13 | **基线缺口** | 尺寸基线只有 musl x86_64 与 Windows x86_64；macOS arm64 / x86_64、Linux gnu 没有首跑数据 | #16 | CI 首跑后补 `ci/size-baseline.json` |
| C14 | **语义澄清** | #15 说 `[auto]` 由 svcd 推断；#22 说 `[auto]` = enabled。两者可兼容，但文档要写死 | #15、#22 | 以 §5.2 为准：`[auto]` = svcd 推断的开机自启，不是插件自报，也不是重启策略 |
| C15 | **命名冲突（已解）** | 票面默认「包根目录 `config` 文件」，但 #13 的包里 `config/` 是目录，不能同名共存 | #13、#20 | 默认改为 `config/config`；显式字段可指向任意 `files[]` 内普通文件 |
| C16 | **已澄清（服务模型根）** | 原服务模型只写 kind core/plugin，没定义 core 怎么来；core 容易被误当成 .spk 的一种 | #9、#13、#14 | **已定（2026-11-12）**：core = `services.d/*.json` 或 `services.d/<id>.sh|bat|cmd|ps1` 的脚本/可执行文件；plugin = 标准 `.spk`；两者共用监督/依赖/日志，但能力面只归 plugin；install/config/export 只对 plugin（§1.6 / §1.7）。**2026-11-13 补充**：schema 2 的 plugin 也可声明 `lifecycle`，复用 core 的 start/stop/status 执行路径；来源与信任边界不变（§1.8）。 |

### 仍未处理

- **C2（推迟）**：桌面 Linux gnu、macOS x86_64、各平台安装包仍不在门禁里；30MB 目前只对 Windows 有意义。维护者 2026-11-12 决定往后放，实施打包流水线时再补。
- **C11（未定）**：`svc ui` 的界面内容；C12 / C13 是低优先的版本 pin 与基线补齐。
- C1 / C3 / C4 / C5 已在 2026-11-12 定；C6 / C7 / C8 / C9 / C10 / C14 / C15 已澄清或属实现约束。

## 14. 不在本规格

- 商城（Web/桌面客户端、审核、签名分发、按架构过滤）——见地图 Out of scope。
- ipk2spk、AI 建插件流水线——见地图 Out of scope。
- TLS 终止、多用户/角色、配置加密、svcd 自升级——v1 不做；要做单开票。

## 15. 修订记录

- 2026-11-12：C1 `platform` only、删除 `arch`，`schema: 1` 重写；C2 推迟；C3 增加 `POST /mcp`；C4 增加 `/api/v1` 管理 API（覆盖全部 `svc` 命令）；C5 明确 v1 不支持 TLS。
- 2026-11-12（补）：core/plugin 边界——core = `services.d/*.json` 的启动脚本/可执行文件，plugin = 标准 `.spk`；install/config/export 只对 plugin。
- 2026-11-12（补）：core 退出工具面——定义不再接受 `capabilities`；新增可选 `lifecycle.start/stop/status`，`svc start/stop/status` 调用钩子；`details` 增加 `probe`。
- 2026-11-12（补）：脚本服务 drop-in——`services.d/<id>.sh|bat|cmd|ps1`，`start/stop/status` 参数入口；同 id JSON 优先；`.ps1` 走 ExecutionPolicy Bypass。
- 2026-11-13（补）：脚本动作改为可选——脚本声明哪个动作，svcd 才调用哪个；缺 `start` 从头执行、缺 `stop` 由框架终止进程、`status` 只把 stdout 作为 `details.probe` 的细节补充。
- 2026-11-13（补）：服务树即进程组——Unix 独立进程组、Windows per-service job object；组内任一进程存活即 `Running`，stop 终止整棵树。
- 2026-11-13（补）：信任根可管理——`svc trust list|add|remove`、`GET/POST/DELETE /api/v1/trust`、Web 控制台上传 `.pub`；包内不携带信任根，编译期 store 公钥仍未内置。
- 2026-11-13（补）：脚本插件——`schema: 2` 增加 `lifecycle` / `interpreter`；脚本 payload 必须声明解释器（版本可空），安装时解析并校验，运行时统一经解释器执行；core drop-in 脚本可原样加 manifest 转成 plugin；`plugin bundle --from-core` 自动生成 lifecycle；老 daemon 拒绝 schema 2。

## 16. 实施票（2026-11-12 拆出）

由本规格拆成 **16 张 tracer bullet**，全部 `ready-for-agent`，阻塞边用 GitHub 原生依赖表示，frontier 起点只有 [#28](https://github.com/webxxxxx/plugin-manager/issues/28)。

| 票 | 交付 | 阻塞于 | 骨架现状（尺寸夹具） |
| --- | --- | --- | --- |
| [#28](https://github.com/webxxxxx/plugin-manager/issues/28) | 注册表与 status | — | 已实现：services.d 扫描、status payload、CLI 接线 |
| [#29](https://github.com/webxxxxx/plugin-manager/issues/29) | 前台监督 core 服务 | #28 | 已实现：proc/wait、事件循环、定时器、UDS/命名管道管理通道；stderr 继承，日志归 #35 |
| [#30](https://github.com/webxxxxx/plugin-manager/issues/30) | 安装/运行 .spk（含 `plugin` 入口） | #28 #29 | 验签/`files[]`/解包/布局/插件扫描/`plugin` 入口已实现；hooks 已实现（60s、环境清空、post-install 按策略回滚）；`install -` / downgrade / prune 未做 |
| [#31](https://github.com/webxxxxx/plugin-manager/issues/31) | 插件 IPC 与工具面 | #30 | 已实现：NDJSON 分帧（1 MiB）、id 路由、64 在途、30s 取消、`registry.tools` / `registry.call`；崩溃摘能力并失败在途调用；128 排队未做 |
| [#32](https://github.com/webxxxxx/plugin-manager/issues/32) | `mcp` 桥 dual-era | #31 | 已实现：modern `server/discover`/`_meta`/缓存字段；legacy `initialize`/`ping`/`list_changed` 轮询；连不上 svcd exit 1 |
| [#33](https://github.com/webxxxxx/plugin-manager/issues/33) | Web UI 反代 | #30 #31 | 已实现：静态资源、base 注入、ETag/304、SPA fallback、`/rpc` 桥；Range/WS/SSE 不做 |
| [#34](https://github.com/webxxxxx/plugin-manager/issues/34) | 配置流 | #30 | 模板/活动副本/`config.new`/`editconfig`/`configCheck`/`.spkb` 导入导出已实现；HTTP 端点归 #39 |
| [#35](https://github.com/webxxxxx/plugin-manager/issues/35) | 日志与 `svc logs` | #29 | 无 |
| [#36](https://github.com/webxxxxx/plugin-manager/issues/36) | 安装为系统服务 | #29 | 无 |
| [#37](https://github.com/webxxxxx/plugin-manager/issues/37) | HTTP 管理 API | #29 #30 | 服务/插件/版本端点已实现；loopback、无 auth；#38/#39 待补 |
| [#38](https://github.com/webxxxxx/plugin-manager/issues/38) | LAN 鉴权与会话 | #37 | token/登录/cookie/CSRF/限速与 listener 重绑已实现；`/mcp` 归 #40 |
| [#39](https://github.com/webxxxxx/plugin-manager/issues/39) | HTTP 配置/导入导出/日志 | #34 #35 #37 | 无 |
| [#40](https://github.com/webxxxxx/plugin-manager/issues/40) | HTTP `/mcp` | #32 #38 | 已实现：modern-only `POST /mcp`、镜像头/Bearer/Origin/1 MiB 校验、与 stdio 共用 `serve_modern`；无 session/SSE |
| [#41](https://github.com/webxxxxx/plugin-manager/issues/41) | 桌面 `svc ui` | #33 #37 | 无 |
| [#42](https://github.com/webxxxxx/plugin-manager/issues/42) | 发行包与签名 | #41 | 无 |
| [#43](https://github.com/webxxxxx/plugin-manager/issues/43) | 嵌入式发行 | #29 #33 | 无 |

> 后续追加：[#57](https://github.com/webxxxxx/plugin-manager/issues/57) `svcd --install / --uninstall` 自安装/卸载（复制四入口 + 调用 install-service；默认保留数据，`--purge` 清理）。

**已打通的端到端路径**：`services.d/*.json` → `svcd run --foreground` → 真实进程（spawn / 就绪探针 / 退避 / stop）→ `svc list/status/start/stop` 经 UDS（Unix）/命名管道（Windows）；插件安装/IPC/工具面、MCP 桥（stdio 与 HTTP）与 HTTP 管理 API 已接通。其余行的系统服务安装、发行、桌面与嵌入式仍是骨架。
