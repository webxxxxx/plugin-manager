# 目录收敛与配置物化

## Status

Accepted（2026-11-18）。收窄 [ADR-0019](0019-config-is-derived-once-at-the-composition-root.md) 派生的 `Paths` 形状与配置面；保留 [ADR-0003](0003-password-auth-and-config-dir.md) 的密码语义、[ADR-0004](0004-install-dir-for-read-only-rootfs.md) 的 `install_dir` 用途、[ADR-0017](0017-windows-machine-wide-config.md) 的 Windows 机宽配置。

## Context

- 配置面有三层重复：`Paths` 8 个字段、`svcd.conf` 6 个路径键、6 个 `SVCD_*`，而且优先级有三种形状——`plugins_dir`/`services_dir` 没有环境变量，`run_dir`/`trust_dir` 没有配置键，`install_dir` 只挪 data 与 logs。运维无法从“配了哪个”推出“实际用哪个”。
- **两个解析器**：`Paths::resolve` 与 `selfinstall::RuntimeDirs::from_settings`（install.rs:374-396）各有一条优先级链。后者让配置文件里的 `install_dir` 赢过 `SVCD_DATA_DIR`/`SVCD_LOG_DIR`，前者相反。同一份输入能得出两套目录。
- **install 有系统默认行为**：没有 `--conf` 也没有 `--prefix` 时它不写任何配置，直接用 `ambient_paths(None)` 的平台默认（install.rs:379）。安装完的文件系统里没有一份能说明“数据在哪”的文件。
- **物化不完整**：生成的默认配置只写死 4 个键（`config_dir`、`plugins_dir`、`services_dir`、`temp_dir`，install.rs:252-323）；`logs_dir` 每次启动重新推平台默认，`run_dir`/`trust_dir` 从来不在文件里。
- **`config_dir` 混放运行时文件**：两个控制 socket、`http.port`、上传暂存 `tmp/`。只读根文件系统上 socket 绑定会失败（ADR-0004 的场景）。
- **首启不一致**：`ensure_conf` 生成配置后，本次进程继续用生成之前的文档（daemon.rs:88-93），生成的 conf 下次启动才生效。
- `user_mode()` 每次调用现算、不缓存（dirs.rs:12-18）。把默认值物化进文件，等于把“谁初始化的”固化进路径。

## Decision

- **三个根，其余全派生**：

  | 根（可配） | 派生 |
  |---|---|
  | `config_dir` | `svcd.conf`、`services.d/`、`trust.d/`、`compat.conf` |
  | `data_dir` | `plugins/`、`temp/`、`run/`（控制 socket + `<id>.json` 记录） |
  | `logs_dir` | `svcd.log`、`<id>.log`、`<id>-hooks.log`、`stats.log` |

  `Paths` 的字段保留（`services_dir`/`plugins_dir`/`trust_dir`/`temp_dir`/`run_dir` 照旧是字段），但只从根派生；派生值不接受单独配置。
- **配置键只剩四个**：`config_dir`、`data_dir`、`logs_dir`、`install_dir`。`install_dir` 只表示安装前缀，不再在运行期推任何路径——它在配置初始化时被折算成三个根的值写进文件。
- **路径环境变量只剩三个**：`SVCD_CONFIG_DIR`、`SVCD_DATA_DIR`、`SVCD_LOG_DIR`。删掉 `SVCD_TEMP_DIR`、`SVCD_RUN_DIR`、`SVCD_TRUST_DIR`。每个配置键另有 `SVCD_<KEY>` 覆盖（2026-11-20 追加，见文末 Amendment）。
- **删掉的两个键与三个变量按“告警并忽略”处理**：启动时打一行 `WARN` 说明被忽略并给出实际派生路径，不改写用户的文件；发布说明给替换表。旧配置缺根键时则相反——补齐一次并写回文件，写回失败只告警。
- **物化是唯一默认来源**：生成的配置写全三个根的绝对值，派生目录不进配置、只在注释里列出解析结果；`svcd --install` 在没有配置文件时也生成一份（不再“什么都不写”）。运行期不再计算平台默认，只读文件。
- **一个解析器**：install 与 daemon 都走 `Paths::resolve`；`RuntimeDirs::from_settings` 的独立优先级链删除。
- **首启即生效**：`ensure_conf` 生成配置后，本次启动用它，不再沿用生成之前的文档。
- **`config_dir` 只放输入**：`http.port` 与上传暂存移到 `data/`（前者随 socket 进 `run/`，后者进 `temp/`）；socket 从 `config_dir` 移到 `data/run`。

## Considered options

- **四个根（run 独立）**：`/run` 是 tmpfs，语义更正统；但持久记录今天就在 `<data>/run`，拆出独立 run 根会逼着状态快照搬家，而接管依赖这些记录，迁移期有重复拉起服务的风险。三个根只挪 socket，不碰记录。
- **保留运行时文件在 `config_dir`**：改动最小，但只读根文件系统上 daemon 直接起不来，ADR-0004 支持的部署形态会被破坏。
- **缺键直接失败**：最严格，但升级时所有旧配置同时失效；物化 + 一次性补齐能在“文件可读”和“不炸现场”之间折中。

## Consequences

- 一份配置文件就能回答“路径在哪”；install 产出的目录与 daemon 实际使用的一致。
- 跨身份复用同一份配置会沿用文件里的路径（用户态初始化、root 使用），这是物化的必然结果，需要在运维文档里写明。
- `svcd --install` 多了“写配置”这个副作用；没有 `--conf` 的安装不再依赖环境里的平台默认。
- 旧键被静默忽略会有一次性的迁移成本；告警里必须给出实际路径，否则现场无从判断。
- 受影响测试：`paths.rs` 的五条优先级测试、`settings.rs` 的路径键测试、`install.rs` 的生成配置与 `--prefix` 测试、`tests/install.rs` 的 install/uninstall 与 user-mode 测试。`Paths::under` 的测试布局要跟新派生关系对齐。
- 不在本决定内：per-source 配置、日志级别与统计；非路径环境变量原本也不在，后由 Amendment 纳入。

## Clarification

- 本文说的“物化”只针对目录。其余默认值（`bind`、限额、超时）仍按“配置里有就用、没有就用编译期默认”，因为它们不需要在运行期重新推导路径。
- `user_mode()` 仍是进程级判定，用于**配置初始化时**选择默认根；判定结果不写进文件——文件里的绝对路径已经表达了它。

## Amendment

2026-11-20（#179）：用户要求所有配置项都能用环境变量覆盖。路径根变量仍只有
`SVCD_CONFIG_DIR`、`SVCD_DATA_DIR`、`SVCD_LOG_DIR`；除此之外，任意
`svcd.conf` 键 `<key>` 都可用 `SVCD_<KEY>`（键名大写）覆盖，环境变量优先于
文件，空值视为未设置。覆盖只在 `settings::load` 时解析一次，未知的
`SVCD_*`（`SVCD_USER`、测试钩子等）不会被当作配置键。被删除的
`SVCD_TEMP_DIR` / `SVCD_RUN_DIR` / `SVCD_TRUST_DIR` 仍是告警并忽略的旧变量，
不是新键的别名。

2026-11-20（#179 后续）：用户要求指定 `install_dir` 时三个根全部挂在它下面。
配置初始化补缺根时，`install_dir` 现在同时提供：

- `config_dir = <install_dir>/config`
- `data_dir = <install_dir>/data`
- `logs_dir = <install_dir>/logs`

显式根（文件里的 `config_dir`/`data_dir`/`logs_dir`）和 `SVCD_*` 仍然优先；
`install_dir` 只补缺失值，不会覆盖已写死的根。安装后的服务定义与 CLI 除了
`<program dir>/svcd.conf` 外，也按 `<program dir>/config/svcd.conf` 发现配置，
所以带前缀的安装不依赖 `SVCD_CONFIG_DIR` 也能自包含。该 Amendment 收窄
ADR-0004 原先“不让 `install_dir` 决定 `config_dir`”的结论：现在它决定默认值，
但 `config_dir` 仍可单独配置到别处。

