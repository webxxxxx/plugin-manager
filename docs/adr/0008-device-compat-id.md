# 设备兼容标识：compat.devices

外挂内核模块的插件只能跑在按同一份内核 ABI 编出来的固件上，而 `platform` 只是用户态目标三元组：SR6110 上任何 `aarch64-unknown-linux-musl` 包都会通过校验，包括按另一份 vendor 内核（私有结构体补丁）编的 `.ko`。vermagic 和符号 CRC 对「结构体布局被改过」并不敏感，装上去不会报错，加载后访问才崩。

## Decision

- **设备给一个兼容代 id**：`device = sr6110`（或 `sr6110-v2`），写在 `<config_dir>/compat.conf`，文件内容只有这一项。id 标识的是「内核/固件兼容代」，不是型号字符串；**只要为旧 id 编的 ko 在新固件上会坏，就必须换 id**。
- **插件包声明支持的 id 列表**：manifest（schema 3）`compat.devices = ["sr6110", ...]`。规则只有四条：
  - 不写 `compat` → 通用包，只受 `platform` 约束；
  - 写了 → 本机 `device` 必须精确出现在列表里；
  - 列表存在但本机没有 id → `Unknown`（无法判定，不是不兼容）；
  - `platform` 仍精确匹配，与设备列表是「且」的关系。
- **校验位置**：安装（验签/结构校验之后、停任何服务之前）、daemon 启动扫描（把已装包标成 `Failed`）、`svc start` 前（防御性）。不匹配的安装不落盘、不停旧版本。
- **含 `.ko` 的包必须写非空列表且用 schema 3**：schema 1/2 的包老 daemon 会忽略列表；含内核模块却不声明设备的包直接拒装。
- **`svc compat` 导出事实**：文本或 `--json`；`svc compat init --device <id>` 给没有 marker 的存量设备补写（原子写、root、0644）。`GET /api/v1/compat` 返回同一结构，供页面/商城做本地筛选。
- **打包侧**：`plugin bundle --compat <device[,device...]>` 写入列表并提升 schema；含 `.ko` 却没给 `--compat` 直接报错。
- 匹配只做精确字符串比较：**没有通配符、前缀、范围或 any-of 交叉**。

## Considered Options

- **platform 字符串拼后缀**（`aarch64-…-musl+sr6110`）：零 schema 变更，但错误信息差、无法多值、打包工具都要懂后缀；只适合过渡。
- **profile + kernel release/ABI/userland 多维约束**：表达力最强，但每加一维就多一组交叉语义和运维矩阵；对「同型号只有一套内核」的现场是过度设计。以后需要时按「出现的字段都必须满足」追加，不推翻本模型。
- **运行时自动探测 ABI**（CRC/符号集/`/proc/version` 哈希）：SR6110 的私有结构体改动不会被这些指标反映，误判比不判更危险；只保留 `uname` 做展示。
- **通配符 `"*"`**：与「不写就是通用」重复，且引入优先级问题；拒绝。
- **允许安装但标警告**：含 ko 的包会真的崩内核，必须 fail closed；不做 `--force`。

## Consequences

- 设备 id 由固件/部署写入；已刷好的设备用 `svc compat init --device <id>` 手工补，写错 id 会放行本该被拒的 ko，因此 id 的权威值应在仓库文档里按「型号 + 固件代」维护。
- marker 在 `/etc` overlay 上，重刷 rootfs 不丢；清 `/data` 会丢，需要重新补。它不属于插件导出内容（换设备会带错 ABI）。
- 缺 marker 时：通用包照常安装/启动；受限包在列表里显示「无法判定」、安装被拒并提示 `svc compat init`。
- 换固件导致 id 变化后，已装包在 daemon 重启时变 `Failed`，需要装新包或卸载；修好 marker 后 `svc start` 可直接恢复。
- 商城按同一个 `compat::evaluate` 做本地筛选：通用包对所有同 platform 设备可见，受限包只在列表包含本机 id 时可用；不把设备事实发给远端。
