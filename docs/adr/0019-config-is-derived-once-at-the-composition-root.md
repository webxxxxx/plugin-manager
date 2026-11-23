# S7 配置来源：运行配置在组合根派生一次

## Status

Accepted。细化 [docs/architecture.md](../architecture.md) §8 的 S7 行；切片 S7a（`Config` 骨架 + `Paths`，#157）与 S7b（`Settings`/`Limits` 收口，#158）已交付。与 [ADR-0011](0011-service-contract-is-the-only-composition-point.md)（服务契约是唯一组合点）同向：配置也只在一个组合点产生。

## Context

- 配置今天是一个**进程级全局**：`settings::{SETTINGS, CONFIG_PATH}`（`OnceLock`，首次调用才绑定）、`limits::LIMITS`（从上面派生）、`paths` 的六个自由函数（每次调用重查 `SVCD_*` 与全局）、`log::SVCD_LOG`（进程级 sink）。全仓约 300 处读取：`paths::` 95、`limits::` 69、`settings::` 41、日志调用 116。
- 因为全局不可注入，**每个模块各自造了一个测试后门**：`blob::test_temp_dir`、`package::TEST_SIGNING_KEY/TEST_TRUST`、`paths::TEST_PLUGINS_DIR`（thread-local）、`runtime::TEST_ROOT`（Mutex）。五种形状，互不一致。
- 因为模块读进程级 env（62 处 `env::var`，其中 `paths` 6、`settings` 1），测试只能靠 `SVCD_*` 当缝；`docs/testing.md` 自己写着「进程级 env + 全局缓存会让并行测试互相踩」。
- 结果：要覆盖依赖目录/限额的模块行为，只能写集成测试并 **spawn 一个真 daemon**；§12 step 5 的目标「模块级测试不再起 daemon」因此无法达成。

## Decision

- **运行配置是一个只读值，名字叫 `Config`**：`Settings`（解析后的 `svcd.conf`）+ `settings_path`（这次实际加载的配置文件；没有则 `None`）+ `Paths`（目录集）+ `Limits`（由前两者派生）。术语见 `GLOSSARY.md`。
- **传值，不传 trait。** 进程启动后配置就是常量（`limits` 的既有注释已说明改文件需重启）；文件与 env 的差异收敛在 bootstrap 一处，不需要动态分发，也不需要「懒绑定」。
- **两个构造入口。** `Paths::resolve(&Settings)` 用于生产：平台默认 → `SVCD_*` → 配置根（未写时由 `install_dir` 折算）覆盖，一次算完，字段全是 `PathBuf`，没有 `Option`、没有 getter 里的 fallback。`Paths::under(root)` 用于测试：同一组目录结构落在一个临时根下，不读进程环境。
- **注入深度：类型持有、函数收窄。** 组合根把 `&Config`（或 `Arc<Config>`）交给各子系统；`paths::config_dir()` 这类自由函数消失，调用点改读 `config.paths.config_dir`；一个函数只需要目录就只收 `&Paths`，不需要整个 `Config`。
- **env 只在 bootstrap 读。** `paths` 的 6 处与 `settings` 的 1 处收进 bootstrap；模块内部不再读进程 env。删掉 `SVCD_*` 覆盖是另一件事（集成测试有 20/19/16 处依赖 `SVCD_DATA_DIR`/`CONFIG_DIR`/`LOG_DIR`），另开票。`install.rs` 的 `SVCD_TEST_*` 是 CLI 自己的测试钩子，不在本决定范围。
- **bootstrap 按需调用、失败硬错。** 四个进程入口（`daemon` / `svc` / `plugin` / `mcp`）在真正需要目录时调同一个 `Config::bootstrap(conf)`；配置损坏就退出，不静默回退默认值。`--help` / `--version` / `uninstall` 这类不需要目录的命令不 bootstrap。
- **`log` 保持进程级 sink。** 记录器本来就是每进程一个，注入到 116 处没有收益；唯一的接线点是 `log::init_svcd(&config.limits, &config.paths.logs_dir, clock)`，级别与开关在同一处派生（ADR-0022）。
- **落点。** 新模块 `src/config.rs` 映射 `platform/config`（rank 0，与 `settings`/`limits` 同层）；§12 step 6 目录重排时纯搬到 `src/platform/config.rs`。

## Slices

- **S7a**：`Config` 骨架 + `Paths::{resolve, under}` + 四批迁移（1 `blob/plugin/package` → 2 `daemon/slot/hook` → 3 `face/transfer/main` → 4 `svc/install/plugin_cli`）+ contract（删六个 `paths::*` 与五个测试后门）+ 验收样本（2 个新模块级测试、1 个集成测试降级为单测）。
- **S7b**（阻塞于 S7a）：`Settings`/`Limits` 收口——删 `settings::current()` / `limits::current()` 两个 `OnceLock`，41 + 69 处改从 `Config` 来。

## Clarification (S7a, #157)

- `Config::bootstrap` is the hard-error entry: the `SVCD_*` reads live in `Paths::resolve`, called from bootstrap and from the single tolerant `config::ambient_paths()`. That second entry exists only for phases that must outlive a broken `svcd.conf`: the daemon's pre-`ensure_conf` directory, `stop` / `shutdown`, install / uninstall and the Windows SCM readiness probe (resolved once, after `--conf`). Every other entry bootstraps and fails hard.
- The repository had **eight** directory getters (`config` / `services` / `temp` / `run` / `plugins` / `trust` / `logs` / `data`), not six; all eight were removed by the S7a contract.
- `settings::current()` / `limits::current()` stayed until S7b (removed by #158, see below); `Config` already carries the bootstrap snapshot so S7b only had to move the readers.

## Clarification (S7b, #158)

- `settings::{SETTINGS, CONFIG_PATH}` and `limits::LIMITS` are gone. `settings::load(conf)` resolves and parses the document once and returns `(Settings, Option<PathBuf>)`; `Config::bootstrap` is its only caller (the daemon resolves its pre-`ensure_conf` `Config` through that same entry). `Limits` is a pure projection (`limits::derive`) and carries no cache.
- Readers receive the value through construction: a subsystem that already holds `Config` reads `config.limits` / `config.settings`; a function that needs only one of them takes `&Limits` (or the already-derived value) as a parameter. `settings_path` write-back (`svc lan passwd`) uses that field instead of guessing `config_dir/svcd.conf`.
- The daemon still needs the config directory before `ensure_conf` can report the path it settled on. It bootstraps once before `ensure_conf` and keeps that document for the whole run; `ensure_conf` only chooses or creates the file the service unit and the next start use. This preserves the old cache's precedence (`SVCD_CONFIG_DIR` > program-dir `config/svcd.conf` > program-dir `svcd.conf` > platform default) instead of letting `ensure_conf`'s fallback silently switch documents mid-start, and it removes the second parse S7a still had on first start.
- `config::ambient_paths(conf)` remains the single tolerant entry for phases that must outlive a broken config; it now takes the explicit `--conf` (when it exists) so the Windows SCM probe and the daemon agree.

## 修订（2026-11-23，#267）

`Config` 在组装时把 `SVCD_<key>` 快照成 `env: BTreeMap<&'static str, String>` 一并带走；`GET /api/v1/settings` 报来源时读这份快照，不再每个请求 `std::env::var`。环境变量的读取仍然只发生在组合根，与本文的「派生一次」一致。

## Consequences

- 模块级测试可以直接构造 `Config`（或 `Paths::under(tmp)`）+ 临时根，不再 spawn 进程；测试之间不再通过进程 env 互相踩。
- 配置错误的失败点从「第一次读取时」提前到 bootstrap；CLI 在配置损坏时硬错（`--help`/`--version` 例外）。
- 约 300 处调用点要迁移；按 [docs/agents/wide-refactor.md](../agents/wide-refactor.md) 的 expand–contract 与 `tools/codemod.mjs` 的清单 + 编译门分批推进。
- `settings_path` 让回写命令（`svc lan passwd`）不再靠拼 `config_dir/svcd.conf` 猜路径。
- 不在本决定范围：S8 时钟、删除 `SVCD_*` 覆盖、`log` 注入、目录重排（step 6）。
