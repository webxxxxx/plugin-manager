# svcd 架构总图

> 状态：v1.0（2026-11-16）。命名已定，见第 15 节与 ADR-0011 / ADR-0012。
> 这份文档是全组唯一的「总图」：加模块、加协议、判断一处改动该落在哪一层，先看它。
> 词表见 [GLOSSARY.md](../GLOSSARY.md)；已定决策见 [docs/adr/](adr/)。

## 0. 一句话

svcd 是一个**服务的运行时**：把定义或 `.spk` 变成服务，让服务在本机活着，把多个服务组合成新服务，并能决定它们在多台机器上怎么摆放。

「服务」是唯一的名词（`kind` 有 core 与 plugin 两种），其余都是动词。

## 1. 两个轴

**横向**是系统对服务做的四件事；**纵向**是代码的六个层。

| 层 | 管理 manage | 监督 supervise | 编排 orchestrate | 集群 cluster |
|---|---|---|---|---|
| 装配点 | `daemon.rs` / `main.rs`：把下面所有层装起来，不含业务 | | | |
| **L5 对外面** | 命令 | 状态查询 | 流的定义 | 节点列表 |
| **L4 服务器** | `control`（命令行通道）· `http` · `mcp` · 准入——**一行共用** | | | |
| **L3 领域** | `manage` | `supervise` | `orchestrate` | `cluster` |
| **L2 内核** | 契约 · 目录 · 调用 · 失败 · 投影——**一行共用** | | | |
| **L1 线格式** | 消息信封 · 契约线（stdio/raw）· 控制线 · WS 帧——**一行共用** | | | |
| **L0 平台** | 路径 · 日志 · 配置 · 进程/线程 · 设备兼容 · 内容存储——**一行共用** | | | |

三个直接推论：

1. **只有 L3 被切成四块**，其余行整行共用。所以顶层目录约 11 个，不是 4×6 个——共用的东西只该有一份。
2. **越往下越稳定**：路径解析十年不变，编排语法一定会改。所以依赖只能从上往下。
3. **L4 只有一行**：命令行、网页、MCP 都从这一层进。加一种协议只动 L4 和 L5。

## 2. 六层：知道什么，不知道什么

| 层 | 知道 | **不知道**（更重要） |
|---|---|---|
| L0 平台 | 文件在哪、怎么起进程、配置文本长什么样 | 「服务」这个词 |
| L1 线格式 | 字节怎么切成一条消息、消息怎么写成字节 | 这条消息是什么意思 |
| L2 内核 | 服务契约、能力、一次调用、失败类别、预算 | 进程、socket、HTTP、文件 |
| L3 领域 | 怎么把定义变成服务、怎么让它活着、怎么组合、怎么摆放 | HTTP 头，JSON 字节 |
| L4 服务器 | 连接、协议握手、鉴权、流式 | 插件是什么 |
| L5 对外面 | 哪个 URL 或哪条命令对应哪个能力 | 不拥有连接、不持有资源 |

**依赖**：A 调用 B 就叫「A 依赖 B」，箭头指向 B。单向 = 只能 A→B。

**为什么必须单向**：今天 `api.rs` 同时是 L4 的引擎、L5 的端点、L1 的帧机，3719 行；改一个协议要动三层。如果 L3 也去读 HTTP 头，将来加 gRPC 就要再改一次业务。

## 3. 四个动词

| 动词 | 做什么 | 今天 |
|---|---|---|
| **管理** manage | `.spk` 或定义 → 服务；改配置、卸载 | 有 |
| **监督** supervise | 让服务在本机活着：启停、崩溃重启、daemon 重启后接管 | **有（唯一跑通的）** |
| **编排** orchestrate | 多个服务组合：并行/串行/汇合/定时；组合体本身又是一个服务 | 无 |
| **集群** cluster | 决定服务在哪台机器上运行 | 无 |

`storage` 不是动词，也不是一个领域：它是这四件事共用的**调用基础设施**——大参数不进帧、只传引用（`$blob`），真正的字节存在平台层的内容存储里。见第 5、6 节。

## 4. 契约：全系统唯一的连接点

**服务契约** = 一个服务对外承诺的东西：能力面、调用信封、生命周期义务、健康语义、它可能给出的失败类别。

只要实现契约，就是服务。于是：

| 实现方式 | 谁负责 | 状态 |
|---|---|---|
| 本机进程（stdio） | `supervise` | 有 |
| 内置（svcd 自己的 core 服务） | `supervise` | 有 |
| 复合流（一条编排） | `orchestrate` | 无 |
| 远程节点 | `cluster` | 无 |

由此得到三条免费的性质：

- **复合服务不需要新机制**：它只是「用编排实现的服务」，在目录、能力面、监督眼里和普通服务一样。
- **递归免费**：复合服务里可以再放复合服务。
- **远程对编排透明**：编排只认契约，不知道背后是本机还是远程。

## 5. 目录

```
src/
  main.rs                     多入口分派（argv[0]）
  daemon.rs                   装配点：装配所有模块 + 事件循环 + 关停

  kernel/                     L2 内核（四动词共用，只有定义与判定）
    mod.rs
    contract.rs                 服务契约：能力面、调用信封（含 $blob 引用）、生命周期义务、健康
    registry.rs                 注册表：定义、能力索引、依赖与环检测
    invoke.rs                   一次调用：超时、重试、取消、预算
    failure.rs                  失败类别 + 可重试性 + 汇合聚合
    projection.rs               能力面 → 各对外面的唯一投影源

  manage/                     L3 领域·管理
    mod.rs                      唯一对外接口
    model/  manifest.rs  ustar.rs  layout.rs  hooks.rs
    check/  verify.rs  trust.rs  compat.rs  interpreter.rs
    ops/    install.rs  uninstall.rs  import.rs  export.rs  bundle.rs  scan.rs

  supervise/                  L3 领域·监督
    mod.rs
    model/  entry.rs  machine.rs  record.rs  definition.rs
    check/  restart.rs  backoff.rs  oneshot.rs  reconcile.rs
    ops/    slot.rs  adopt.rs  rescan.rs  hook.rs  script.rs

  orchestrate/                L3 领域·编排（第一个文件出现时才创建）
    model/  step.rs  flow.rs  trigger.rs  fanin.rs
    check/  dag.rs  mapping.rs  budget.rs
    ops/    executor.rs  trigger.rs  composite.rs

  cluster/                    L3 领域·集群（同上）
    model/  node.rs  placement.rs
    check/  eligibility.rs
    ops/    remote.rs  heartbeat.rs  failover.rs

  wire/                       L1 线格式：字节 ↔ 消息
    mod.rs
    message/  rpc.rs  envelope.rs        共享消息信封
    contract/ stdio.rs  raw.rs           daemon ↔ 插件实例
    control/  channel.rs  client.rs      daemon ↔ 命令行
    ws/       frame.rs  accept.rs

  server/                     L4 服务器：协议会话
    mod.rs
    control/  listener.rs  session.rs    命令行通道也是服务器
    http/     engine.rs  routes.rs  takeover.rs
    mcp/      session.rs  transport.rs  progress.rs
    auth/     gate.rs  token.rs  session.rs

  face/                       L5 对外面：端点与投影
    mod.rs
    api.rs           /api/v1
    plugin_ui.rs     /plugin-ui（静态 + 实时 + 控制台）
    mcp_tools.rs     能力面 → tools/list · tools/call
    blob.rs  install.rs  export.rs

  cli/                        L5 对外面：命令行
    mod.rs  svc.rs  plugin.rs  status.rs

  platform/                   L0 平台：宿主资源与 OS 能力
    mod.rs
    paths.rs  log.rs  config.rs  compat.rs
    host/  proc.rs  threads.rs  wake.rs  zerocopy.rs
    storage/  meta.rs  path.rs  upload.rs  part.rs  gc.rs    内容寻址存储（原 blob）

  selfinstall/                特例：宿主领域（注册服务、PATH、卸载器）
    mod.rs  plan.rs  execute.rs  service_win.rs
```

`orchestrate/` 与 `cluster/` 现在不建：**目录在第一个文件出现时创建**，不建空壳。

`platform/storage` 是内容寻址存储（原 `blob.rs`）：上传会话、分片、过期回收。它和 `log` 的文件轮转同类——都是技术资源，所以归平台，不归领域。领域侧只需要契约能表达「这个大参数是 `$blob` 引用」。

## 6. 四种模块种类

| 种类 | 模块 | 内部形态 | 允许依赖 |
|---|---|---|---|
| 领域 | manage · supervise · orchestrate · cluster | `model/` → `check/` → `ops/` | kernel、wire、platform |
| 编解码 | wire | `message/` · `contract/` · `control/` · `ws/` | platform |
| 适配 | server · face · cli | 不分层，保持薄 | 全部领域 + wire + kernel |
| 内核 / 平台 | kernel · platform | 平铺，只有定义与判定 | platform（kernel）／无（platform） |

领域模块内部三段，顺序不能反：

- `model/` 类型、解析、不变量 —— 最稳定，先写
- `check/` 纯函数：校验、策略、上限 —— 不碰资源，最好测
- `ops/` 真正干活：起进程、写文件、发请求 —— 最薄，最容易变

三个特例，别硬套：

- **内核**只有定义与判定（契约、目录、失败类别、预算），不许出现进程、socket、文件。
- **平台**可以持有技术资源及其回收（日志轮转、临时文件、内容存储），但**不许出现服务、能力、调用这类领域概念**。
- **`selfinstall`** 是宿主领域（注册服务、改 PATH、卸载器），内部只有 plan（算要做什么）→ execute（做），并且把「需要重启 daemon」作为**结果返回**，不许反调装配点。

## 7. 依赖规则（四条不变量）

1. **只能从上往下。** 层由目录种类决定，不允许反向。
2. **同层领域模块之间只许「查」与「共享定义」，不许「替对方做」。** 监督可以读目录里有没有这个服务，不能替 manage 执行安装。
3. **L4 不认识 L3 的领域概念；L5 不拥有连接。**
4. **`platform` 可以持有技术资源及其回收，但不许出现领域概念；`kernel` 不许出现进程、socket、文件。**

今天的三处真实反例（用来验证规则不是空话）：

- `slot`（L3 监督）调用 `mcp::tool_error`（L5 的错误格式）——领域调对外面。
- `transfer`（本应是 manage 的一次导入动作）反向依赖 `package` / `plugin` / `status` / `channel`。
- `install`（自身安装）直接调 `daemon::shutdown_quiet`——下层调装配点。

## 8. seam 地图

**seam** = 一个可以整体换掉实现、调用方不用改的位置；判断标准是**今天有几个真实实现**（一个 = 想象中的 seam）。

| # | seam | 两侧 | 今天几个 adapter | 状态 |
|---|---|---|---|---|
| **S1** | **服务契约** | 内核 ↔ 四种运行实现 | 2（本机进程、内置）→ 未来 4 | 未开，契约藏在 `slot`/`package`/`registry` |
| S2 | 调用传输 | 内核 ↔ 载体 | 1（本机 stdio） | 暂不开，但信封必须先定 |
| S3 | 线格式实现 | 线格式 ↔ 平台 | 2（unix poll / windows 线程） | 未开，`cfg(unix)` 散在业务文件里 |
| S4 | 请求交接 | 服务器 ↔ 对外面 | 6+（两种结果：响应 / 交出连接） | 未开，现在是 5 个硬编码钩子 |
| S5 | 失败投影 | 领域 ↔ 对外面 | 3（HTTP / CLI / MCP） | 未开，靠字符串匹配 |
| S6 | 进程与管道 | 监督 ↔ 平台 | 2（unix / windows job） | 未开 |
| S7 | 配置来源 | 所有模块 ↔ 配置 | 1 生产 + 测试 | 未开，全局 OnceLock |
| S8 | 时钟 | 监督/编排 ↔ 时间 | 1 生产 + 测试 | 未开 |
| S9 | 准入 | 服务器 ↔ 认证 | 4（token / bearer / cookie / 本机免认证） | 未开，四分支挤在 `lan` |

开 seam 的顺序：**S1（含 S5 失败类别）→ S4 → S3 → S6 → S7/S8**。S9 随时可做，S2 等集群真正开始再开。

## 9. 新文件放哪：三个问题

```
1. 它在「说协议」还是在「做事」？
   说协议 → server/（会话）或 wire/（字节）
   做事   → 问 2

2. 它是「四种运行载体的公共规则」还是「某一种载体的做法」？
   公共规则   → kernel/
   某一种载体 → manage/ supervise/ orchestrate/ cluster/
   技术资源   → platform/（例如内容存储、日志文件）

3. 在动词目录里，它是类型解析、纯判定，还是流程？
   类型解析 → model/   纯判定 → check/   流程 → ops/
```

对外面额外一条：**`face/` 与 `cli/` 只做「URL 或命令 → 能力」的翻译，不写流程**；有流程就回第 2 问。

## 10. 扩展剧本

| 要加的东西 | 改哪里 | 不需要动 |
|---|---|---|
| 一个 HTTP 端点 | `face/xxx.rs` + `server/http/routes` 一行 | 引擎、领域、其他面 |
| 一个流式端点 | 同上，返回「交出连接」 | 引擎（今天要改两个引擎） |
| 一种新协议（gRPC / MQTT / WS 控制面） | 新建 `server/xxx/` + `face/xxx_*.rs` | supervision、manage、编排、集群 |
| 一种汇合策略 | `orchestrate/model` + `check` | 执行器、契约、监督 |
| 一种触发器 | `orchestrate/ops` + `server` 一个入口 | 流模型、复合 |
| 一种运行载体（WASM / 容器） | 新模块实现契约 | 管理、编排、集群、对外面 |

## 11. 今天的文件怎么进这棵树

| 今天 | 将来 | 拆点 |
|---|---|---|
| `package.rs` `bundle.rs` `trust.rs` `interpreter.rs` | `manage/` | 按 model / check / ops 三段拆 |
| `plugin.rs` | `manage/`（布局与扫描）+ `kernel/contract.rs`（能力声明） | 声明与操作分开 |
| `slot.rs` `supervisor.rs` `registry.rs` `runtime.rs` `hook.rs` `script.rs` `definition.rs` | `supervise/` + `kernel/registry.rs` | 注册表类型进内核，运行载体留监督 |
| `blob.rs` | `platform/storage/`（内容寻址、上传会话、回收） | `$blob` 引用语义归 `kernel/contract` |
| `ipc.rs` `rpc.rs` `rawhub.rs` `reactor.rs` | `wire/contract/` + `wire/message/` | 帧、消息、连接分开 |
| `channel*.rs` | `wire/control/` + `server/control/` | 字节在线格式，会话在服务器 |
| `api.rs` | **四块**：`server/http/` + `face/api.rs` + `wire/ws/` + `kernel/failure.rs` | 它今天横跨三层 |
| `mcp.rs` `mcp_http.rs` `catalog.rs` | `server/mcp/` + `face/mcp_tools.rs` | 传输契约与能力投影分开 |
| `blob_http.rs` `install_http.rs` `export_http.rs` `ui_http.rs` `webui.rs` | `face/` 下同名词，去掉 `_http` | 只留投影 |
| `svc.rs` `plugin_cli.rs` `status.rs` | `cli/` | — |
| `install.rs` `service_win.rs` | `selfinstall/` | plan / execute |
| `paths.rs` `log.rs` `settings.rs` `limits.rs` `proc.rs` `threads.rs` `wake.rs` `zerocopy.rs` `compat.rs` `auth.rs` | `platform/` | 技术底座 |
| `main.rs` `daemon.rs` | 原地 | 装配点 |

## 12. 迁移顺序

| 步 | 动作 | 验收 |
|---|---|---|
| 0 | `tools/dep-lint` 只观察，先报出当前所有违规 | 得到基线红线清单 |
| 1 | 砍向上边：`is_privileged`→platform、`target()`→platform、`MAX_SCHEMA`/`MAX_PROTOCOL`→kernel、`Restart`/`Status`→kernel、`definition→registry` 归位 | lint 少 6 条红线，测试全绿 |
| 2 | 契约从 `slot`/`package`/`registry` 抽出（S1 + S5 失败类别） | 编排可以用 fake 契约开始写 |
| 3 | 抽 `server/http` + `server/mcp` + 请求交接（S4） | `api.rs` 降到 1500 行以下 |
| 4 | 线格式与进程（S3 / S6） | Windows 支持变成「加一个 adapter」 |
| 5 | 配置与时钟（S7 / S8） | 模块级测试不再起 daemon |
| 6 | 目录按本文件重排（纯搬家） | 编译与测试全绿 |

## 13. 不做什么

- **不切多个 crate**：36k 行、单二进制、交叉编译目标多；目录 + lint 已经给到九成收益。
- **不做通用 HTTP 框架**：`server/http` 只认两种结果（内存响应、交出连接）、三个动作（接连接、派发、交出）。
- **不给每个模块硬套三层**：`wake`(48 行)、`rpc`(88)、`paths`(252) 就是单文件。
- **不引入事件总线或 DI 容器**：配置用显式参数；一个实现的 seam 不开。
- **不动 ADR 已定的语义**：接管、oneshot、重启前清理、未签名安装、token 校验时机。

## 14. 既有 ADR 落在哪

| ADR | 落点 |
|---|---|
| 0001 单一 multicall 守护进程 | 装配点 `daemon.rs` / `main.rs` |
| 0002 生命周期与解释器 | `manage/model`（声明）+ `manage/check`（解释器解析） |
| 0003 密码与配置目录 | `platform/config` + `server/auth` |
| 0004 只读根文件系统上的 install_dir | `selfinstall/plan` + `platform/paths` |
| 0005–0007 接管 / 一次性服务 / 重启前清理 | `supervise/model` + `supervise/check` + `supervise/ops` |
| 0008 设备兼容标识 | `platform/compat` |
| 0009 未签名安装 | `manage/check` |
| 0010 bind 与插件 UI token | `server/auth` |

## 15. 已定的命名与决策

命名（2026-11-16 定）：

1. 四个动词：**管理 / 监督 / 编排 / 集群**（manage / supervise / orchestrate / cluster）。
2. 内核层：**kernel**；技术底座：**platform**。
3. 内容存储不是独立领域，归 `platform/storage`；`$blob` 引用语义归 `kernel/contract`。

两条 ADR：

- [ADR-0011](adr/0011-service-contract-is-the-only-composition-point.md) **服务契约是唯一组合点**：管理生产契约实例，监督 / 编排 / 集群是实现契约的三种运行载体，复合与远程靠「实现同一契约」获得，不靠特判。
- [ADR-0012](adr/0012-protocol-is-server-plus-face.md) **协议 = 服务器 + 对外面**：服务器做会话，对外面做端点投影；新增协议不改现有代码。

下一步（按风险排序）：

1. 设计 **S1 服务契约**的接口形状（能力面、调用信封、失败类别、生命周期义务）——它错了，后面三个动词一起返工。
2. 写 `tools/dep-lint` 基线，先报出当前所有违规，不改代码。
3. 把第 12 节的六步切成 4–6 张 tracer bullet 工单（进 GitHub Issues），逐张实现。
