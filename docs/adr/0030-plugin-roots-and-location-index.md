# 插件根与位置索引

## Status

Accepted（2026-11-25）。给 [ADR-0023](0023-directory-convergence-and-materialized-config.md) 的
`plugins_dir` 派生关系加一个出口：它可以是一组根；`Paths` 的字段随之从单值变成列表，
并新增一份位置索引。

## Context

- 插件卷和系统卷常常不是同一个：设备上是 30 MiB 的内部 `/data`（空闲 17.5 MiB）加一张
  SD 卡。单根时 15.7 MiB 的包装完就没有升级余量——升级要同时放下旧版与新版。
- ADR-0023 把 `plugins_dir` 收敛成 `<data_dir>/plugins`，理由是"一条路径链、一份配置就能
  回答路径在哪"。多根放宽的是"只能有一个"，不是"要多条链"：仍然只有一份列表、一处解析。
- 多根带来三个新事实：同一个 id 可能在两个根上；一个插件的目录不再能从列表顺序推出来；
  跨卷 `rename` 不成立（EXDEV），staging 必须和落点同卷。

## Decision

- `plugins_dir` 恢复为可配的**逗号分隔根列表**，顺序即优先级；不配置等于
  `<data_dir>/plugins`。相对路径按配置文件所在目录解析；重复项去重；空项报错且不写盘。
  `SVCD_PLUGINS_DIR` 是同一份覆盖（ADR-0023 Amendment 的 `SVCD_<KEY>` 规则）。
- `Paths.plugins_dir: PathBuf` → `Paths.plugin_roots: Vec<PathBuf>`，构造后永不为空；
  `default_plugin_root()` 返回 `[0]`。
- **位置索引** `<data_dir>/plugin-locations.json`：`{version, plugins:{<id>: <root>}}`。
  - 写：先写临时文件再 `rename`，崩溃只会留下旧文件或新文件。
  - 读：文件缺失、坏 JSON、版本不符都按"没有索引"处理，扫描重建；daemon 不因为一份
    缓存文件坏了就不启动。
  - 索引是提示，磁盘是真相：每次扫描按根重建，与文件不一致就改回磁盘的版本。
  - 索引里的根即使已经不在配置列表里也照样使用——改了列表不该让已装插件凭空消失。
- **扫描走所有根**。同一个 id 出现在两个根上 → 一条冲突告警（点名两个绝对路径），
  第一个根胜出，**不自动删任何一边**；根不可用时它的条目保留。
- 安装成功后写索引。写失败只告警：文件已经提交，下一次扫描会把它补回来。
- 列表漏掉 `<data>/plugins`、而那里确实有插件时，启动告警给出目录与插件数。不自动搬，
  也不改用户的文件。
- **选根规则**（唯一一处实现，`manage/ops/install_plugin/root`）：
  1. 已装插件留在索引记录的根上，请求里给了另一个根就是 `409`，不是悄悄装第二份；
  2. 显式根（`--root` / `?root=`）必须出现在 `plugins_dir` 里，否则 `400` 并列出可选根；
  3. 其余情况按"**装得下且剩余空间最小**"挑：实测空闲字节升序，取第一个 `free >= needed`；
     量不到的根（没挂载、没权限）不参与比较，也不会被猜成"空";
  4. 都不够时拒绝，并把每个根的空闲字节写进消息。
- **staging 落在选中根里**：`StagingInstaller` 在拿到 manifest（id 与总大小已知）之后才
  创建 `<root>/.staging-…`，因为上传前还不知道 id。`commit_staged` 的 `rename` 因此
  永远同卷；`plugin.install_staged` 从 staged 树的父目录**读回**这个根，不重新选一次。
- `.spkb` 备份包不再需要外层暂存目录：`plugin.spk` 与配置都在内存里，内层包自己选根。

## Considered options

- **新键 `plugin_roots` 与旧的 `plugins_dir` 并存**：能避免"旧文档里的 `plugins_dir`
  突然生效"这种迁移意外，但两个只差一个 `s` 的键会长久留在文档与现场里；而真正会踩到的
  现场是"列表漏了 `<data>/plugins`"，用启动告警（带目录与插件数）处理更直接。旧的
  `plugins_dir` 在 ADR-0023 之后只打印告警，很少有配置还写着它。
- **扫描永远包含 `<data>/plugins`**（不要求它出现在列表里）：插件不会突然消失，但"安装
  落点集合"与"扫描集合"变成两个概念，而且运维把内部 flash 从列表里去掉的意图会被无视。
  选择"必须显式列出 + 漏了就告警"。
- **用 `<data>/plugins/<id>` 符号链接表达多根**：省掉索引，但 Windows 创建符号链接需要
  管理员权限，卸载与搬家的语义也更绕。
- **索引放进 `config_dir`**：它是运行期状态、每次扫描可重建，和 `temp`/`run` 一样属于 data。

## Consequences

- `Paths` 的每个调用点要么用 `plugin_roots`（扫描、staging、Windows ACL），要么用
  `default_plugin_root()`；"按 id 找目录"继续走 `kernel::layout::plugin_dir`，它现在读索引。
- 一个 id 只能落在一个根上；往另一个根装同一个 id 会被拒绝（409）。
- 新装默认去剩空间最小的根，所以设备上 15.7 MB 的包装进 17.5 MiB 的 `/data`，
  SD 卡留给更大的包；升级仍在原根上，装不下就明确拒绝，由调用方决定先卸载。
- 根不可用（SD 卡拔了）时：索引条目**保留**、列表里少一行、启动时一条告警点名被隐藏的
  插件；卸载与导出返回 409 并带上那个根（不是 404 "没有这个插件"），`uninstall --force`
  只删记录、不碰卷；`plugin where <id>` 直接回答"在哪个根、那卷在不在"。插回卡后下一次
  扫描把它找回来。
- 索引文件是缓存，不进备份；删了只会让下一次启动重建。
- 受影响测试：`paths.rs`（列表、顺序、漏根告警）、`settings.rs`（列表解析、空项）、
  `config_doc.rs`（`plugins_dir` 重新进表并从 `REMOVED_PATH_KEYS` 移出）、
  `config_file.rs`（生成配置的注释）、`tests/http/config.rs`（旧键告警改举
  `services_dir`）。
