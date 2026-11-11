# CI 三平台产物体积门禁：cargo-zigbuild / cross / musl-gcc 三条交叉编译路线，以及「≤1 MB」「≤30 MB」怎么量

调研日期：2026-11-12。问题：Rust 服务管理器（svcd）+ 插件管理器要产出
`x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl`、`armv7-unknown-linux-musleabihf`
三个 musl 静态产物，外加 Windows / macOS 原生产物，全部跑在 GitHub Actions 上；
硬约束是嵌入式 ≤1 MB、桌面 ≤30 MB。**哪条路线最省事、最可复现，体积怎么变成 CI 门禁？**

**取证方式说明**：本笔记全部来自一手来源（crates.io API、上游仓库 README / CI 配置 /
commit、rustc book、Rust Reference、Cargo book、GitHub Actions runner-images 仓库与
Debian 包库）。**没有做任何本地构建或体积实测**——没有在本机编译 musl 目标，没有跑
`cargo zigbuild`/`cross`，没有量过任何二进制。凡属推断都标了「推断」，凡查不到的进
「未确认项」。所有体积数字都不是本笔记测出来的。

## 结论（先给答案）

1. **推荐路线：三个 musl 目标全部在 `ubuntu-24.04`（写死，不用 `ubuntu-latest`）上用
   `cargo-zigbuild` + Zig 链接**，`cargo zigbuild --release --target <triple>` 一条命令
   出三个架构；Windows / macOS 各用原生 runner 出本地产物。不装 QEMU、不用 `build-std`、
   不用 nightly、不在 Windows 上交叉到 Linux。
2. **只编译不需要 QEMU**：QEMU 只在「运行/测试目标架构的二进制」时才需要，而且
   ubuntu-24.04 镜像**不自带** qemu-user——`cargo-zigbuild` 自己的 CI 就是先
   `apt-get install qemu-user qemu-user-binfmt` 才能跑 `cargo test --target aarch64-…`。
   你的门禁只量体积，所以这一步可以整段删掉。
3. **`cross` 已经不是「省事」选项**：最新 release 停在 **v0.2.5（2023-02-11）**，仓库仍
   在活跃维护（2026-10 还在把 musl 目标升到 musl 1.2.5 / GCC 14.3.0），但版本号不反映
   代码状态 → 要用就得装 git 版或未发布的镜像。它换来的是「能在容器里跑目标架构测试」，
   代价是 Docker 拉镜像 + 缓存重。
4. **`musl-gcc` 路线只对 x86_64 成立**：Debian/Ubuntu 的 `musl-tools` 只提供**本机架构**的
   `musl-gcc` wrapper；aarch64 / armv7 要另建 musl 交叉工具链（musl-cross-make /
   crosstool-ng），工作量与可复现性都最差。**排除。**
5. **体积门禁量「strip 后的可执行文件字节数」**：这是唯一跨平台一致、可复现、和
   「≤1 MB」语义对得上的量。`strip = "symbols"`（等价 `strip = true`）是唯一能大幅降体积
   的开关；`opt-level = "z"` **不一定**比 `"s"` 小（官方原话）；`lto = "fat"` +
   `codegen-units = 1` + `panic = "abort"` 值得一起开。
6. **阈值用十进制**：1 MB = `1_000_000` 字节，不要用 1 MiB（1,048,576，宽了 4.9%）。
   同时把每次体积写进 artifact 并与**基线文件**比（涨 2% 就红），否则「一直在 1 MB 以内」
   会掩盖缓慢膨胀。
7. **三个目标都是 Tier 2 且自带 std**（aarch64-musl / x86_64-musl 还带 host tools），
   `rustup target add` 三行就够，**不需要 `build-std`**，也不需要自定义 target spec。
   musl 版本（1.2.5）由 rustup 的预编译 std 固定。
8. **推荐路线里最容易翻车的一环：`armv7-unknown-linux-musleabihf` 用 Zig 链接**——
   上游 CI 只测了 `armv7-unknown-linux-gnueabihf`（glibc）和 `aarch64-unknown-linux-musl`，
   **没有测 armv7 + musl**，而 32 位 ARM 正是 `zig cc` 认 triple / `compiler_rt` 缺符号
   这类问题最集中的地方。次易翻车：浮动的 `-latest` 标签（`ubuntu-latest` 公告要在
   2026-11 切到 26.04；`macos-latest` 现在只有 arm64）。

## 证据

### A. 链接器与交叉编译工具现状

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `cargo-zigbuild` 最新 **0.24.1**，2026-11-04 发布，MIT，要求 rustc ≥ 1.88；2026 年内发了 0.23.0(06-16)/0.23.4(09-02)/0.24.0(10-21)/0.24.1(11-04) | 在维护，发布节奏按月计 | https://crates.io/api/v1/crates/cargo-zigbuild | 0.24.1 / 2026-11-04 |
| Caveats #1：「Currently only Linux and macOS targets are supported」 | **不能**用它交叉到 Windows | https://github.com/rust-cross/cargo-zigbuild （README「Caveats」） | main @ 2026-11 |
| 用法：先 `rustup target add <triple>`，再 `cargo zigbuild --target <triple>`；`pip install cargo-zigbuild` 会连带装 `ziglang`；`CARGO_ZIGBUILD_ZIG_PATH` 可指定 zig | 安装面很薄：cargo 插件 + 一个 zig 二进制 | 同上（README「Usage」「Environment Variables」） | main @ 2026-11 |
| 对 glibc 目标 `-C target-feature=+crt-static` **不支持**；README 原话「Use a `*-musl` target instead if you need a fully static binary」 | 全静态 = 必须走 musl target，这与本项目目标一致 | 同上（「Specify glibc version」Note） | main @ 2026-11 |
| zig 0.15+ 自带 libc++ 19 头，用 bindgen 的 crate 需要 clang ≥ 18 | 若依赖里有 bindgen，CI 得装 clang 18+ | 同上（Caveats #3） | main @ 2026-11 |
| 已知上游问题：`zig cc` 不识别部分 Rust triple，例如 `armv7-unknown-linux-gnueabihf`，需 `-mcpu=generic` + 显式 target features（PR #58） | **32 位 ARM 的 triple 映射是已知薄弱点** | 同上（「Known upstream zig issues」#1） | main @ 2026-11 |
| CI 矩阵：os `[ubuntu-latest, macos-15, windows-latest]` × toolchain `[1.88.0, stable, nightly]` × zig `[0.11.0, 0.17.0, master]` | 上游只承诺「当前 stable/nightly + 这几个 zig 版本」的组合 | https://github.com/rust-cross/cargo-zigbuild/blob/main/.github/workflows/CI.yml | main @ 2026-11 |
| 同一 CI 的 musl 用例只有 `aarch64-unknown-linux-musl`；`armv7` 只出现 `armv7-unknown-linux-gnueabihf` | **armv7 + musl 无上游 CI 覆盖** | 同上（「Linux - Test musl libc build」「Linux - Test glibc build」） | main @ 2026-11 |
| 同一 CI 要跑 aarch64 二进制时先 `apt-get install qemu-user qemu-user-binfmt` 并 `update-binfmts` | QEMU 不预装；且只有「运行」才需要 | 同上（「Linux - Test glibc run/test」） | main @ 2026-11 |
| Zig 最新稳定 **0.17.1（2026-11-02）**，0.17.0（2026-10-01）、0.16.0（2026-04-13），master 为 0.18.0-dev | zig 需要自己 pin，CI 里不会自带 | https://ziglang.org/download/index.json | 0.17.1 / 2026-11-02 |
| `cross` 最新 release **v0.2.5**，2023-02-11 | 三年半没有新 tag | https://github.com/cross-rs/cross/releases.atom | v0.2.5 / 2023-02-11 |
| `cross` 仓库仍活跃：2026-10-31 还有合并提交 | 「没发版」≠「没人维护」 | https://github.com/cross-rs/cross/commits/main.atom | 2026-10-31 |
| 2026-10-14 合并：`*-musl` 目标默认工具链从 musl 1.2.3 / GCC 9.2.0 升到 **musl 1.2.5 / GCC 14.3.0**，旧工具链保留为 `:legacy` 镜像 | cross 的 musl 版本现在与 Rust 目标一致（1.2.5） | https://github.com/cross-rs/cross/commit/526d03ddcd2f70e683074b3ea6f75ed300f33132 | 2026-10-14 |
| cross 的 zig 镜像 pin 着 **zig 0.11.0 + cargo-zigbuild 0.23.4**；升级原因是 Rust **1.98.0（2026-08-20）** 起对每个 AArch64 Linux target 传 `-Wl,--fix-cortex-a53-843419`，cargo-zigbuild < 0.23 不认这个参数，链接直接失败 | **必须 pin cargo-zigbuild ≥ 0.23**，否则升 Rust 后 aarch64 全线红 | https://github.com/cross-rs/cross/commit/d6204e62acdba1a117341f93972f3d52470a3052 | 2026-10-24 |
| Debian trixie 的 `musl-tools`（1.2.5-3.1~deb13u1）：「contains the musl-gcc wrapper script **for the native architecture**」 | 原生 musl-gcc 只能出本机架构，跨架构要自建工具链 | https://packages.debian.org/trixie/musl-tools | 1.2.5-3.1~deb13u1 |

### B. Rust 目标本身（三架构都要 std）

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-musl`：Tier 2 **with Host Tools**，musl 1.2.5 | 有官方预编译 std，`rustup target add` 即可 | https://doc.rust-lang.org/rustc/platform-support.html | 2026 版文档 |
| `aarch64-unknown-linux-musl`：Tier 2 **with Host Tools**，musl 1.2.5 | 同上 | 同上 | 同上 |
| `armv7-unknown-linux-musleabihf`：Tier 2 **without Host Tools**，std = ✓，musl 1.2.5 | 能编译，但不能在该架构上跑 cargo/rustc（测试需 QEMU 或真机） | 同上 | 同上 |
| `aarch64-unknown-linux-musl` 目标页：经 rustup 分发、「otherwise requires no special configuration」、「can be cross-compiled from any host」，测试需 ARM64 主机或 QEMU | 交叉编译本身是被官方支持的常规操作 | https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-linux-musl.html | 2026 版文档 |
| Rust Reference：默认静态链接 C runtime 的例外包括 `armv7-unknown-linux-musleabihf`、`x86_64-unknown-linux-musl`（列表是「such as」，**未列** `aarch64-unknown-linux-musl`） | musl 目标普遍静态，但 aarch64 是否默认静态**文档没有明说** → CI 里显式断言 | https://doc.rust-lang.org/reference/linkage.html | 2026 版文档 |
| `-C target-feature=+crt-static` / `-crt-static` 控制 C runtime 静态/动态链接 | 想强制静态就显式加 | https://doc.rust-lang.org/rustc/codegen-options/index.html#target-feature | 2026 版文档 |

### C. GitHub Actions runner 的现状（2026-11）

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 标签表：`ubuntu-latest` = Ubuntu 24.04；`ubuntu-24.04-arm`（arm64）；`windows-latest` = Windows Server 2025（VS2026）；`macos-latest` = **macOS 26 arm64** | 三平台都有原生 runner；macOS 默认是 arm64 | https://github.com/actions/runner-images （README「Available Images」） | main @ 2026-11 |
| macOS 26 **x64** 只能通过 `macos-latest-large` / `macos-26-intel` / `macos-26-large` 拿到（`-large`/`-xlarge` 属 larger runners） | 想免费出 `x86_64-apple-darwin`，应在 arm64 macOS runner 上**交叉**编译，别指望 Intel runner | 同上（含「Label scheme」） | main @ 2026-11 |
| 公告：`ubuntu-latest` 标签将在 **2026 年 11 月**切到 Ubuntu 26.04 | 现在正是切换窗口 → **CI 必须写 `ubuntu-24.04`** | https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md | 2026-11 |
| Ubuntu 24.04 镜像 `20261101.360.1`：Rust/Cargo **1.99.1**、Rustup 1.29.2、Clang 16/17/18、binutils 2.42、`file`、`upx` 4.2.2；**已装软件列表里没有 Zig，apt 列表里没有 qemu** | Rust 预装（但要 pin），Zig 和 QEMU 都要自己装 | https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md | 20261101.360.1 |
| Windows Server 2025 镜像 `20261103.298.1`：Rust/Cargo **1.99.1**、MSVC 2022 x64 工具链、Windows SDK 10.0.26100、LLVM 20.1.8、GNU Binutils 2.46 | Windows 原生 `x86_64-pc-windows-msvc` 开箱即用 | https://github.com/actions/runner-images/blob/main/images/windows/Windows2025-VS2026-Readme.md | 20261103.298.1 |
| macOS 14 系列镜像已于 **2026-11-02** 从 GitHub Actions 退役 | 不要写 `macos-14` | https://github.com/actions/runner-images （README IMPORTANT 段） | 2026-11-02 |
| cargo-zigbuild 的 CI 用 `mlugg/setup-zig@v2`（`use-cache: false`）、`dtolnay/rust-toolchain@master`、`mozilla-actions/sccache-action` | 装 zig / 装工具链 / 缓存都有成熟现成 action | https://github.com/rust-cross/cargo-zigbuild/blob/main/.github/workflows/CI.yml | main @ 2026-11 |

### D. 体积：profile 开关、测量工具、门禁口径

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `strip` 三档：`none` / `debuginfo` / `symbols`；`strip = true` **等价于** `symbols`；release 默认 `"none"` | 不改就是带符号发布 | https://doc.rust-lang.org/cargo/reference/profiles.html#strip | 2026 版文档 |
| `-C strip=symbols` 会连符号表一起剥；官方警告：依赖符号表做 backtrace/profiling 的程序「should usually avoid `-Cstrip=symbols`」 | 门禁产物可以剥，但要留一份**未剥**的给归因工具/崩溃分析 | https://doc.rust-lang.org/rustc/codegen-options/index.html#strip | 2026 版文档 |
| `opt-level`：`"s"` = optimize for size；`"z"` = size 且关循环向量化，但**「Often results in larger binaries than `s`」** | 别默认 `z` 更小，两个都量 | https://doc.rust-lang.org/rustc/codegen-options/index.html#opt-level | 2026 版文档 |
| Cargo book 同调：「`"s"` and `"z"` levels not being necessarily smaller」 | 同上 | https://doc.rust-lang.org/cargo/reference/profiles.html#opt-level | 2026 版文档 |
| `lto = true` 等价 fat LTO；release 默认 `lto = false`、`codegen-units = 16`、`opt-level = 3` | 体积档要显式覆盖这三项 | https://doc.rust-lang.org/cargo/reference/profiles.html#default-profiles | 2026 版文档 |
| `panic` 可取 `abort` / `immediate-abort` / `unwind`；`immediate-abort` = 终止且**不调用 panic hook** | `abort` 稳定可用；`immediate-abort` 更狠 | https://doc.rust-lang.org/rustc/codegen-options/index.html#panic | 2026 版文档 |
| `-Z build-std`：必须 nightly Cargo + nightly rustc，必须装 `rust-src`，默认 std features 是 `backtrace` + `panic-unwind`（`-Z build-std-features` 可覆盖） | 用 stable 就别指望 build-std；它也是「关掉 backtrace/panic-unwind 省体积」的唯一正规入口 | https://doc.rust-lang.org/cargo/reference/unstable.html#build-std | 2026 版文档 |
| Cargo 侧同样把 `panic-immediate-abort`、`trim-paths` 列为 unstable（nightly） | 想用就要接受 nightly 漂移 | https://doc.rust-lang.org/cargo/reference/unstable.html | 2026 版文档 |
| `--remap-path-prefix FROM=TO` 重写输出里所有路径（诊断、调试信息、宏展开） | 去掉构建机绝对路径 = 可复现性的基本操作（stable 可用） | https://doc.rust-lang.org/rustc/command-line-arguments.html#--remap-path-prefix | 2026 版文档 |
| `cargo-bloat` 最新 **0.12.1**（2024-05-10）；支持 ELF / Mach-O / PE；`--crates` 按 crate 归因；`--message-format json`；README 示例里 `.text` 378 KiB 而 **file size 2.8 MiB** | 它解析符号表 → **只能喂未 strip 的产物**（推断）；且它报的 file size ≠ .text | https://crates.io/api/v1/crates/cargo-bloat ; https://github.com/RazrFalcon/cargo-bloat | 0.12.1 / 2024-05-10 |
| `bloaty` 最新 release **v1.1（2020-05-24）**，之后无 tag | 六年没发版，只适合作补充视角 | https://github.com/google/bloaty/releases.atom | v1.1 / 2020-05-24 |
| Rust Reference：`+crt-static` 在某些 target 上会被忽略，建议「inspect the resulting binary」 | 别信假设，CI 里 `file`/`ldd` 断言 | https://doc.rust-lang.org/reference/linkage.html | 2026 版文档 |

## 对技术选型的影响

### 1. 路线对比（结论先行）

| 维度 | cargo-zigbuild + Zig | cross（Docker） | rustup + musl-gcc |
| --- | --- | --- | --- |
| x86_64 musl | ✅ | ✅ | ✅（本机 musl-tools） |
| aarch64 musl | ✅（上游 CI 有覆盖） | ✅ | ❌ 需自建工具链 |
| armv7 musleabihf | ⚠️ 可用但**无上游 CI 覆盖** | ✅（musl 1.2.5 / GCC 14.3） | ❌ 需自建工具链 |
| Windows / macOS 目标 | ❌ 不支持 Windows 目标 | ✅（但等价于再拉一堆镜像） | ✅/⚠️ |
| 需要 QEMU 吗（只编译） | 不需要 | 只在 `cross test/run` 时需要 | 不需要 |
| CI 依赖 | zig 二进制（~55 MB 压缩包）+ cargo 插件 | Docker 守护进程 + 镜像拉取 | 系统 musl 工具链，且只有本机架构 |
| 发布状态 | 0.24.1（2026-11-04），按月发版 | v0.2.5（2023-02-11），靠 main 分支 | 跟随发行版 |
| 可复现性抓手 | pin zig 版本 + pin 插件版本 | pin 镜像 digest | pin 发行版/工具链 |
| 结论 | **推荐** | 备选（要跑测试时才值） | 排除（跨架构不成立） |

### 2. 推荐路线（确定性：高）

**工具 + 目标 + 测量 + 门禁，一条流水线：**

```text
ubuntu-24.04（写死，别用 -latest）
  ├─ rustup target add x86_64-unknown-linux-musl \
  │                   aarch64-unknown-linux-musl \
  │                   armv7-unknown-linux-musleabihf
  ├─ pip install cargo-zigbuild==0.24.1 ziglang==0.17.1   # 或 setup-zig + cargo install --locked
  ├─ cargo zigbuild --release --target <triple> --locked
  ├─ file dist/<triple>/svcd        # 断言 "statically linked" / "ELF 32-bit ... ARM"
  ├─ strip --strip-all（其实 strip="symbols" 已在 profile 里做了）
  └─ stat -c%s dist/<triple>/svcd   # 门禁：<= 1_000_000

windows-2025（写死）
  └─ cargo build --release --target x86_64-pc-windows-msvc
     PowerShell: (Get-Item .\svcd.exe).Length  # 门禁：<= 30_000_000

macos-26（arm64，写死）
  ├─ cargo build --release --target aarch64-apple-darwin
  ├─ rustup target add x86_64-apple-darwin && cargo build --release --target x86_64-apple-darwin
  └─ lipo -create 两个 → universal2（可选）
```

**为什么这样最省事**（确定性：高）
- 三个 musl 目标共用**同一条命令形状**，没有 Docker、没有 QEMU、没有 nightly；
- 上游 CI 恰好覆盖了 `aarch64-unknown-linux-musl` 这条最常用的路径；
- 所有 musl 二进制都出自**同一个 ubuntu job 的同一套工具链**，跨架构体积可比；
- Windows/macOS 用原生 runner，`-msvc` 和 Apple SDK 都不需要你操心。

**为什么不在 Windows 上交叉到 Linux**（确定性：高）
`cargo-zigbuild` 的 Caveats 明说只支持 Linux/macOS 目标；即便技术上能拼出链接器，
你也只是把「Linux 上一条命令」换成「Windows 上一条命令 + 一套自证」——没有任何收益。

**为什么 macOS 不买 Intel runner**（确定性：中高）
`macos-latest` 现在只有 arm64；Intel 镜像挂在 `-large`/`-intel` 标签下，属 larger runners。
`x86_64-apple-darwin` 在 arm64 macOS 上 `rustup target add` + 原生 clang 就能交叉，
一次 job 出两个 arch，再用 `lipo` 合成 universal2。

### 3. 体积门禁怎么定（确定性：高）

**口径：量 `strip` 后的可执行文件字节数。**
- 对 musl 静态单文件，「安装后占用」≈ 文件大小（没有共享库依赖），两者几乎相等；
- 对桌面产物，「安装后占用」= 目录树总和（`.app` bundle 里的资源、Windows 的 VC runtime
  若动态链接、dSYM 若误打包），可能比 exe 大一个量级。**所以门禁只卡 exe/二进制本身，
  目录树总和单独记录、单独报**（确定性：中，属经验判断）。

**阈值（确定性：高）**
```yaml
# 十进制，和「≤1MB」的字面语义对齐
embedded:   1_000_000 B   # ×3 架构，任一超 → fail
desktop:   30_000_000 B   # ×2 平台
warn_ratio: 1.02          # 相对基线涨 2% → 先 warn，两连击才 fail
```
- 用 1 MiB（1,048,576）会白送 4.9% 的余量，不建议；
- 门禁输出**一张表**（target / 字节数 / 相对基线 % / strip 前后对比），失败时直接贴进 job summary；
- 基线文件（如 `ci/size-baseline.json`）随 PR 更新，评审时看得见「这次涨了多少」。

**怎么量、用什么量（确定性：高）**
| 目的 | 工具 | 说明 |
| --- | --- | --- |
| 门禁阈值 | `stat -c%s` / PowerShell `.Length` / `du -b` | 唯一权威数字，不要用 `ls -lh` 的四舍五入 |
| 确认真的静态 | `file`（+ Linux 上 `ldd` 应报 "not a dynamic executable"） | Reference 都说别信假设 |
| section 级归因 | `size`（binutils）/ `llvm-size` | `.text` / `.rodata` / `.data` 分布 |
| crate/函数级归因 | `cargo bloat --release --crates --message-format json` | **必须用未 strip 的构建**，否则符号没了（推断） |
| 发布包大小 | `tar --zstd` 或 `zip -9` 后的字节数 | 与「用户下载多少」对齐；musl 静态二进制的压缩比通常很高（推断） |
| 补充视角 | `bloaty`（2020 后无新 release） | 只在需要分段/按符号细分时用 |

**典型 Cargo.toml 写法（确定性：高）**
```toml
[profile.release]
opt-level     = "z"      # 与 "s" 对比实测，官方说 z 不一定更小
lto           = "fat"    # = true
codegen-units = 1
panic         = "abort"
strip         = "symbols"  # = true；发布用，归因另建一份未 strip 的
debug         = 0
incremental   = false

# 桌面端可以只剥 debuginfo，保留符号以便崩溃分析
[profile.desktop]
inherits = "release"
strip    = "debuginfo"
```
- `opt-level` 要在 `"s"` / `"z"` 之间**实测**，别抄；
- `panic = "abort"` 去掉 unwinding 支持，通常能省 `.eh_frame`/landing pad（推断，未实测）；
- `codegen-units = 1` + fat LTO 是「用编译时间换体积」的标准组合，代价是 CI 变慢（推断）。

### 4. 可复现性清单（确定性：高，逐条都是一手可查）

1. `rust-toolchain.toml` 写死 `channel = "1.99.1"`（或你选的版本），别依赖 runner 预装的 1.99.1；
2. `Cargo.lock` 入库，CI 用 `--locked`；
3. `cargo-zigbuild` **精确版本**（`=0.24.1`），且 **≥ 0.23.0**（否则 Rust ≥ 1.98 的
   `-Wl,--fix-cortex-a53-843419` 会让 aarch64 链接直接失败）；
4. Zig 精确版本（`0.17.1`），用 `setup-zig` 的 `version:` 或 `pip install ziglang==…`；
   不要 `master`；
5. `RUSTFLAGS` 里加 `--remap-path-prefix=$GITHUB_WORKSPACE=/build` 之类，去掉构建机路径；
6. **不要** `-C target-cpu=native`（README 明确警告 SIGILL 风险，且完全不可复现）；
7. 不要给 musl 目标加 `target-feature`（保持默认），除 `+crt-static` 这一项用于断言；
8. musl 版本你**控制不了**：它来自 rustup 的预编译 std（1.2.5）。Zig 只为 C 依赖提供
   libc/启动文件，**zig 换版本可能让体积微变**——这就是第 4 条为什么要 pin。
9. 想要字节级可复现，前三件事（pin toolchain / pin zig / pin 插件）+ remap 路径就够；
   再往上要 `SOURCE_DATE_EPOCH` 之类的规范化，属于额外工程（未在本笔记验证）。

### 5. `build-std` 要不要用（确定性：高：**不要**）

- 三个目标都自带 std，`rustup target add` 即可；`build-std` 是 nightly-only，还得装
  `rust-src`，且要传给**每一次** cargo 调用；
- 它唯一不可替代的用途是「重新编译 std 并把默认 features（`backtrace`、`panic-unwind`）
  关掉」——这是压到 1 MB 以下的最后一张牌；
- 代价：nightly 漂移 + 无法用稳定工具链复现 + CI 时间显著增加。
- **建议：先用 stable + strip/lto/panic=abort 量一次**。如果 x86_64 落在 700–900 KB，
  说明有余量，立刻停手；如果逼近 1 MB 再评估 nightly + `-Z build-std-features=panic_immediate_abort`
  （确定性：中，取决于实测）。

### 6. 缓存与耗时（确定性：低，均为推断）

| 项 | 量级 | 依据 |
| --- | --- | --- |
| 单架构 musl 冷构建（小 workspace） | 2–5 min | 推断：rustc 编译 + zig 首次解包/缓存 |
| 三架构顺序跑在一个 ubuntu job | 6–12 min（冷）/ 2–4 min（热） | 推断：多 target 不复用 rustc 产物 |
| Windows msvc 冷构建 | 3–6 min | 推断 |
| macOS arm64 冷构建（两个 arch） | 4–8 min | 推断：macOS runner 核数少于 Linux |
| 9 个 job 并行的墙钟 | 6–10 min | 推断 |
| 缓存收益 | rust-cache 命中后 30–60% 时间 | 推断；上游用 sccache |

能省的都省：`actions/cache` 的 key 里带上 `target triple`，否则三个架构互相污染；
Zig 的全局 cache（`zig env` 的 `global_cache_dir`）也值得缓存（README 有说明）。

### 7. 最容易出问题的一环（按风险排序）

1. **`armv7-unknown-linux-musleabihf` + Zig 链接**（确定性：中高）
   上游 CI 只测 `armv7-…-gnueabihf`，且 README 已知问题里点名 32 位 ARM 的 triple 映射。
   缓解：① 第一个 PR 就加一个「三架构 hello world」的 smoke job，不要等到主程序；
   ② 断言 `file` 输出 `ELF 32-bit LSB ... ARM, EABI5 ... statically linked`；
   ③ 真挂了就**只对 armv7 换 cross**（cross 的 armv7 musl 是 musl 1.2.5/GCC 14.3），
   而不是整条流水线换。
2. **浮动标签**（确定性：高）
   `ubuntu-latest` 公告 2026-11 切 26.04；`windows-latest` 已经是 Server 2025 + VS2026；
   `macos-latest` 只有 arm64。全部写死版本号。
3. **cargo-zigbuild 版本落后**（确定性：高）
   < 0.23.0 在 Rust ≥ 1.98 上必炸 aarch64。把 `=0.24.1` 写进锁文件/CI。
4. **把 `cargo bloat` 跑在 strip 过的产物上**（确定性：中）
   符号被剥后归因退化成 `[Unknown]`；归因用单独一份未 strip 的 release 构建。
5. **只卡「有没有超」，不卡「涨了多少」**（确定性：高）
   体积会在 1 MB 以内缓慢膨胀到没救；基线 + 2% 告警是廉价保险。

## 未确认项

- **`cargo-zigbuild` 0.24.1 × Zig 0.17.1 的实际组合**：上游 CI 矩阵写的是 `0.17.0`，
  没写 `0.17.1`；patch 版本理论上兼容，但**未实测**。
- **`aarch64-unknown-linux-musl` 是否默认 `crt-static`**：Rust Reference 的例外列表里
  只有 armv7-musl-hf / x86_64-musl / i686-musl / arm-musl，没有 aarch64。要么它默认动态
  （不太可能），要么列表非穷尽。**未确认** → 建议 CI 里显式 `-C target-feature=+crt-static`
  并用 `file` 断言。
- **Zig 链接 musl 目标时，启动文件/`libc.a` 到底来自 Zig 还是 rustup 的 `self-contained/`**：
  没有找到明确的一手说明。如果两者混用，可能出现「musl 版本双源」的隐蔽行为差异。
  **未确认**。
- **所有具体体积数字**：本笔记没有编译任何二进制，`1 MB` 是否够用完全取决于实测。
- **各平台构建时长**：上表全是推断，没有实测样本。
- **macOS runner 的 Rust 版本**：只核到 ubuntu（1.99.1）与 windows（1.99.1）的镜像清单，
  macOS 镜像清单未逐条核对，预装 Rust 这一点属常识性推断。
- **`ubuntu-latest` 是否已经切到 26.04**：公告说 2026-11，本笔记成稿时镜像表仍写 24.04，
  切换可能正在进行。
- **GitHub arm64 runner（`ubuntu-24.04-arm`）的计费范围**：标签存在，但「公共仓库免费」
  的具体条款未核对。
- **cross 是否有 0.3 / 新的发布计划**：仓库活跃但没有 roadmap 证据。
- **`bloaty` 在 PE/Mach-O 上的现状**：v1.1（2020）之后无 tag，是否还能正确解析新格式
  的 Mach-O / PE **未确认**。
- **`panic = "abort"` / `immediate-abort` 对体积的具体收益**：只有语义文档，没有官方
  给出的百分比，属推断。
