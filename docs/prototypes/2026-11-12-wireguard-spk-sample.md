# WireGuard 插件 .spk 样张（prototype）

- 票：[WireGuard 插件 .spk 样张](https://github.com/webxxxxx/plugin-manager/issues/12)
- 日期：2026-11-12
- 目的：用一份真实样张验证 [.spk 包格式](https://github.com/webxxxxx/plugin-manager/issues/13) 的 tar + manifest + signature，以及 [插件 IPC 协议形态](https://github.com/webxxxxx/plugin-manager/issues/4) 的方法集能否撑住一个系统级插件。

## 产物

```
samples/wireguard/
  manifest.json              # 作者字段；files[] 由打包器计算（#50 起 plugin bundle 也读它）
  payload/bin/wg-agent       # python3 的 NDJSON agent（样张，不是真 WireGuard）
  payload/web/index.html     # 经 /plugin-ui/wireguard/rpc 调能力的示例 UI
  config/wg.json             # 配置模板（首次安装复制到配置目录）
  hooks/post-install.sh
  hooks/pre-remove.sh
samples/keys/sample-2026.key # 测试私钥（仅样张）
samples/keys/sample-2026.pub
samples/spk/wireguard-1.0.0-x86_64.spk
samples/spk/wireguard-1.0.0-x86_64/    # 打包暂存（= 解包后的包布局）
samples/spk/install-preview/           # checker 的解包预览
tools/gen_sample_key.mjs
tools/build_spk.mjs
tools/check_spk.mjs
tools/demo_rpc.py
```

## 命令

```sh
node tools/gen_sample_key.mjs      # 只跑一次；--force 轮换
node tools/build_spk.mjs           # 产出 .spk + 暂存目录
node tools/check_spk.mjs samples/spk/wireguard-1.0.0-x86_64.spk --extract samples/spk/install-preview
python tools/demo_rpc.py           # 走一遍 ping/status/up/…/shutdown
```

## 结果（实测）

- 包内 5 个文件；manifest 1,491 B；`.spk` 12,800 B（tar 头 + 512 对齐）。
- checker：7 个条目（manifest + signature + 5 files），Ed25519 验签 OK，manifest 是 canonical 形式。
- 协议 demo：`svcd.ping` → `{"ok":true}`；`status` → `tunnel down`；`up` → `peers 1`；`svcd.status` → `{summary, details}`；未知工具 `-32602`；未知方法 `-32601`；`svcd.shutdown` → exit 0。

## 样张如何映射规格

- **容器**：ustar 头 `mtime=0`、`uid/gid=0`、条目顺序 manifest → signature → 按路径排序的文件；没有压缩。
- **清单**：`schema/protocol/platform/exec/capabilities/ui/files/hooks` 全部出现；`files[]` 钉住 payload、config 与 hooks 的 sha256/size/mode。
- **签名**：`signature.json` 签的是 `manifest.json` 的原样字节；checker 先验签再逐文件验 hash。
- **hooks**：两个脚本都在 `files[]` 里被 hash 钉住；env 只用 `SVCD_*`，cwd 是版本目录。
- **UI**：`ui.entry=index.html` 解析到 `payload/web/index.html`，页面走 `POST rpc`。

## 发现

1. **平台标识缺口（已在 [#18](https://github.com/webxxxxx/plugin-manager/issues/18) 定）**：原 manifest 只有 `arch`，分不出 OS 与 musl/glibc。现改为 `platform` 目标三元组（样张为 `x86_64-unknown-linux-musl`），安装时与设备 `TARGET` 精确相等；`arch` 只从第一段派生，用于文件名与商城前缀过滤。
2. **Windows 上 `mode` 无意义**：NTFS 没有 exec 位，安装器只能把 `mode` 当 Unix 语义，可执行性靠扩展名判断。
3. **信任密钥格式未定**：样张用 PEM SPKI；`trust.d` 收 PEM 还是 raw base64，规格应指定一种。
4. **canonical manifest 很便宜**：排序键 + 紧凑序列化即可；建议把「打包器必须产出 canonical manifest」从纪律升级成规格条文。
5. **hooks 被 `files[]` 覆盖**：样张验证通过，hook 改动会破坏签名。

## 不覆盖

- 不执行真实 WireGuard，不碰内核。
- 不模拟 `keep-old` / `replace` 的真实磁盘行为（checker 只做解包预览）。
- payload 是 python 脚本，不是静态二进制；Windows/macOS 形态未做。
- 不验证聚合层 dual-era（那是 [#6](https://github.com/webxxxxx/plugin-manager/issues/6) 的行为）。
