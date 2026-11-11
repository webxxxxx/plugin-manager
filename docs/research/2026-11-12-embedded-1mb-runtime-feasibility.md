# ≤1 MB musl 静态链接预算，对 svcd 与 PluginManager 这类守护进程现实吗？

调研日期：2026-11-12（Asia/Shanghai）
范围：只回答体积/静态链接可行性问题；进程模型、.spk 清单格式、MCP 协议细节不在本文范围。
方法：能在本机复现的一律实测（附录 A 给出完整命令）；只有无法复现的才引用一手文档。**凡标「实测」的数字都在 2026-11-12 于本机重跑过**，测量环境见附录 A.0。

## 结论

1. **对「单个二进制」1 MB 现实，但窗口很窄，且取决于是否要 TLS。** Rust（`opt-level=z` + LTO + `codegen-units=1` + `panic=abort` + `strip` + 静态 musl）实测：空 hello 364.7 KiB，带信号处理/进程拉起/等待的真实 svcd 形状 424.7 KiB，带清单解析 + HTTP 应答的 PluginManager 形状 496.8 KiB。这些是能跑起来的真实静态 ELF（已在 WSL 里启动验证），**但功能面很薄**（见第 3 条），不要把 496.8 KiB 当成「功能齐全的 PM」。
2. **加上 TLS（rustls + ring）就不再现实。** 同一个 lean PluginManager 二进制从 496.8 KiB 涨到 **1,180.7 KiB**（2.4 倍），仅一个 tokio + rustls 的 TLS accept 循环就是 1,284.9 KiB。**内嵌反向代理若要自己终结 TLS，1 MB 的预算在 Rust 里单这一项就被吃穿。** 因此 1 MB 的前提是：TLS 交给前置 terminator（或复用平台证书库/外部 stunnel），PluginManager 只做明文 HTTP/1.1 与 MCP 聚合。
3. **两条二进制合计 1 MB，只在「功能裁到最瘦」的假设下勉强成立。** 实测 lean svcd 424.7 KiB + lean PM 496.8 KiB = **921.5 KiB**（裸 release），用 `-Z build-std=std,panic_abort` 重编可到 452.7 + ~390.7 ≈ **843 KiB**（后者对 svcd 是估算，按实测 −8% 推算）。**但这 921.5 KiB 的实现是玩具级**：单次 `read()` 的 HTTP、无 keep-alive、无流式代理、无 TLS、无日志/指标、无配置热加载。任何一项补齐都会突破 1 MB——尤其是「内嵌聚合式 MCP Server + 反向代理」这一对需求，本质上要求一个像样的 HTTP 栈。
4. **Go 直接出局，不是「需要优化」而是「差一个数量级」。** Go 1.27.4 `CGO_ENABLED=0` linux/amd64：hello world 默认 2.24 MiB，`-trimpath -ldflags="-s -w"` 后仍 **1.44 MiB**——比整个 1 MB 预算还大。真实 PM 形状（net/http + encoding/json + os/exec + signal）默认 8.40 MiB，strip 后 **5.68 MiB**。而且这个地板在涨：同一台机器上 Go 1.22.0 的 hello 是 1.81 MiB / 1.17 MiB。
5. **C 有 10 倍余量，Rust 有 ~2 倍余量，Go 没有余量。** musl-gcc `-static -Os -s` 实测：hello 13.2 KiB，带 sigaction 的守护进程骨架 13.0 KiB，带 `getaddrinfo` 45.2 KiB，带 `localtime` 29.2 KiB。也就是说「1 MB」这个数字对 C 是宽松约束，对 Rust 是紧约束，对 Go 是不可能约束。**约束真正惩罚的不是「静态 musl」，而是「带 GC 的运行时 + 高层 HTTP 框架」。**
6. **Rust 里最大的单项成本是 HTTP 栈和 TLS，不是语言运行时本身。** 实测（相对 364.7 KiB 的 std hello）：`mio` 只加 8 KiB；`libc` + 进程/信号加 44 KiB；`serde_json` + `serde` derive 加 40 KiB；`tokio` current_thread 加 160 KiB；`tiny_http` 加 196 KiB；`hyper` 加 280 KiB；`axum` 加 456 KiB。选择顺序应当是 std::net 手写 HTTP → tiny_http/hyper → axum，每上一级吃掉 0.2–0.5 MB。
7. **`panic=abort` 单独用几乎不省体积，`build-std` 才是真正的开关。** 实测 panic=abort 对 hello 的收益是 **0 字节**（434,928 → 434,928，因为链接的是预编译好的 unwind 版 std）；改用 nightly `-Z build-std=std,panic_abort` 后 hello 从 373,408 → 344,736 B（−7.7%），pm_lean 从 508,672 → 463,608 B（−8.9%）。**任何「panic=abort 能省 20%」的说法在这个组合下不成立**，除非同时重建 std。
8. **静态 musl 的坑集中在「运行时文件」而不是「链接」——而这一点恰好对我们有利。** 静态 musl 二进制在**完全空的 chroot**（无 /lib、无 /etc）里能正常启动，`getaddrinfo` 不需要 NSS 模块（不需要 `dlopen`）。真正的运行时依赖只有：时区数据（空 chroot 里 `TZ=Asia/Shanghai` 静默退化为 UTC）、以及 `/etc/hosts`+`/etc/resolv.conf` 这类配置。**`dlopen` 在静态 musl 里能链接成功但运行时返回 NULL**——这条直接确认了「插件只能是进程模式，不能是动态库」的约束是硬性的、且在 1 MB 目标下反而是好事。

## 证据

### A. Rust 实测（x86_64-unknown-linux-musl 静态，release）

测量配置统一为 `opt-level="z"`、`lto=true`、`codegen-units=1`、`panic="abort"`、`strip=true`，编译器 rustc 1.99.1（2026-10-13）稳定版，除注明 nightly 者。

| 事实 | 数字/结论 | 来源 URL | 日期或版本 |
|---|---|---|---|
| std 空 hello，默认 release（无 LTO/无 strip） | 4,683,048 B = 4,573.3 KiB | 本机实测（附录 A.1） | rustc 1.99.1, 2026-11-12 |
| 同上，仅 `strip=true` | 434,928 B = 424.7 KiB（−90.7%） | 本机实测（A.1） | 同上 |
| 同上，再加 `panic="abort"` | 434,928 B = 424.7 KiB（**收益 0 字节**） | 本机实测（A.1） | 同上 |
| 再加 `lto=true` 且 `codegen-units=1` | 385,696 B = 376.7 KiB（−11.3%） | 本机实测（A.1） | 同上 |
| 再加 `opt-level="z"`（= 本文基准 hello） | **373,408 B = 364.7 KiB** | 本机实测（A.1） | 同上 |
| 改 `opt-level="s"` 作对照 | 377,504 B = 368.7 KiB | 本机实测（A.1） | 同上 |
| nightly `-Z build-std=std,panic_abort` 重编 std 后 hello | 344,736 B = 336.7 KiB（相对基准 −7.7%） | 本机实测（A.2） | rustc 1.102.0-nightly (2026-11-09) |
| `-Z build-std` 是官方不稳定特性，需 nightly + rust-src | 需 `rustup +nightly target add x86_64-unknown-linux-musl` 才能提供自带的 crt/libc.a | https://doc.rust-lang.org/cargo/reference/unstable.html#build-std | 2026-11-12 查阅 |
| 守护进程形状：std + `libc`，sigaction(SIGTERM/SIGINT) + spawn 子进程 + `try_wait` 轮询 + `nanosleep` | 418,464 B = 408.7 KiB（相对 hello +44.0 KiB） | 本机实测（A.1） | rustc 1.99.1 |
| svcd 形状：std + `libc`，tab 配置文件解析 + 拓扑排序 + 重启策略 + 信号退出 | **434,848 B = 424.7 KiB**（+60.0 KiB） | 本机实测（A.1） | rustc 1.99.1 |
| PluginManager 形状：std + `libc` + `serde`/`serde_json`，.spk 清单解析 + 架构校验 + 拉起插件 + 单次读 HTTP JSON-RPC 应答 | **508,672 B = 496.8 KiB**（+132.1 KiB） | 本机实测（A.1） | rustc 1.99.1 |
| 同 pm_lean，但用 `-Z build-std=std,panic_abort` 重编 | **463,608 B = 452.7 KiB**（−8.9%） | 本机实测（A.2） | rustc 1.102.0-nightly |
| `mio`（os-poll + net）最小事件循环 | 381,600 B = 372.7 KiB（+8.0 KiB） | 本机实测（A.1） | mio 1.x |
| std 手写 HTTP/1.1（`std::net::TcpListener`，无第三方 crate） | 406,176 B = 396.7 KiB（+32.0 KiB） | 本机实测（A.1） | rustc 1.99.1 |
| `tokio`（rt + net + signal + time + process + io-util + macros）current_thread | 537,456 B = 524.9 KiB（+160.2 KiB） | 本机实测（A.1） | tokio 1.x |
| 同上但 `rt-multi-thread` | 582,512 B = 568.9 KiB（+204.2 KiB） | 本机实测（A.1） | tokio 1.x |
| `tiny_http` 回声服务 | 574,224 B = 560.8 KiB（+196.1 KiB） | 本机实测（A.1） | tiny_http 0.12.x |
| `hyper` 1.x + hyper-util（http1 server） | 660,272 B = 644.8 KiB（+280.1 KiB） | 本机实测（A.1） | hyper 1.x |
| `axum` 单个 JSON 路由 | 840,528 B = 820.8 KiB（+456.2 KiB） | 本机实测（A.1） | axum 0.8.x |
| `rustls`(ring) + tokio TLS accept 循环 | **1,315,704 B = 1,284.9 KiB**（+920.2 KiB） | 本机实测（A.1） | rustls 0.23.x / ring |
| lean PM 加 `rustls`(ring) 做 TLS 终结 | **1,209,064 B = 1,180.7 KiB**（相对 496.8 KiB 的 2.4 倍） | 本机实测（A.1） | rustls 0.23.x |
| 段级拆解（`llvm-size -A`，.text） | hello 242,541 B；svcd_lean 339,629 B；pm_lean 327,995 B；tokio_cur 399,073 B；axum 595,448 B；pm_lean_tls 764,683 B + .rodata 257,836 B（ring 的 crypto 常量表） | 本机实测（A.3） | rustc 1.99.1 自带 llvm-size |
| 三个 lean/带 TLS 二进制均为真静态 | `ldd` 输出 `statically linked`，可在空 chroot 内启动 | 本机实测（A.4） | WSL2 / Debian, kernel 6.18 |

### B. Go 实测（linux，CGO_ENABLED=0，全静态）

| 事实 | 数字/结论 | 来源 URL | 日期或版本 |
|---|---|---|---|
| hello world，默认构建 | 2,350,492 B = **2.24 MiB** | 本机实测（附录 A.5） | go1.27.4, 2026-11-12 |
| hello world，`-trimpath -ldflags="-s -w"` | 1,507,488 B = **1.44 MiB**（已超 1 MB） | 本机实测（A.5） | go1.27.4 |
| 同机对照：Go 1.22.0 的 hello（默认 / strip） | 1,895,268 B = 1.81 MiB / 1,228,952 B = 1.17 MiB | 本机实测（A.5） | go1.22.0 |
| PluginManager 形状（net/http + encoding/json + os/exec + os/signal + net.Listen）默认 | 8,808,749 B = **8.40 MiB** | 本机实测（A.5） | go1.27.4 |
| 同上 strip 后 | 5,951,648 B = **5.68 MiB** | 本机实测（A.5） | go1.27.4 |
| 同上 strip 后，linux/arm64 / linux/386 | 5,505,184 B = 5.25 MiB / 5,828,768 B = 5.56 MiB | 本机实测（A.5） | go1.27.4 |
| 当前最新稳定 Go 版本 | go1.27.4（本文测量所用） | https://go.dev/dl/?mode=json | 2026-11-12 查阅 |
| `-s -w` 的语义：省略符号表与 DWARF 调试信息 | 官方 flag 文档 | https://pkg.go.dev/cmd/link | 2026-11-12 查阅 |
| Go 纯 Go 解析器 vs cgo 解析器：`CGO_ENABLED=0` 走纯 Go 解析器，直接向 resolv.conf 里的服务器发 DNS 请求，不调用 `getaddrinfo`/NSS | 官方 net 包文档原文：“It can use a pure Go resolver that sends DNS requests directly to the servers listed in /etc/resolv.conf, or it can use a cgo-based resolver that calls C library routines such as getaddrinfo and getnameinfo.”，可用 `GODEBUG=netdns=go` 或 `-tags netgo` 强制 | 本机 Go 源码 `src/net/net.go`（go1.27.4）；亦见 https://pkg.go.dev/net#hdr-Name_Resolution | go1.27.4 |
| TinyGo 是否可用于本项目的常驻 HTTP 守护进程 | 官方包支持表把 `net/http` 列为 **✗ no**（不支持） | https://tinygo.org/docs/reference/lang-support/stdlib/ | 2026-11-12 查阅 |

### C. C 与 Zig

| 事实 | 数字/结论 | 来源 URL | 日期或版本 |
|---|---|---|---|
| C hello（`puts`），`-static -Os -s` | **13,544 B = 13.2 KiB** | 本机实测（附录 A.6） | musl-gcc (GCC) 13.0.0 |
| C 守护进程骨架：`sigaction` + `sleep` 循环 | **13,288 B = 13.0 KiB** | 本机实测（A.6） | musl-gcc 13.0.0 |
| C + `getaddrinfo` | 46,320 B = 45.2 KiB | 本机实测（A.6） | musl-gcc 13.0.0 |
| C + `localtime_r`（时区） | 29,944 B = 29.2 KiB | 本机实测（A.6） | musl-gcc 13.0.0 |
| C + `dlopen` | 34,752 B = 33.9 KiB（能链接，运行时返回 NULL，见 D 节） | 本机实测（A.6） | musl-gcc 13.0.0 |
| musl 官方对静态体积的口径 | “Minimal static-linked binaries can be under 10 kB of code, even with threads, and even useful programs can be under 50 kB.” | https://musl.libc.org/about.html | 2026-11-12 查阅 |
| musl 官方对静态链接可部署性的口径 | “Binaries statically linked with musl have no external dependencies, even for features like DNS lookups or character set conversions that are implemented with dynamic loading on glibc. An application can really be deployed as a single binary file” | https://musl.libc.org/about.html | 2026-11-12 查阅 |
| Zig 同规模守护进程体积 | **未实测**（本环境无法取得 Zig 工具链，见「未确认项」）。可核对的锚点：Zig 默认对 `x86_64-linux-musl` 生成静态 musl 二进制，其运行时体积量级与上表 C 同级（C 基准 13.0 KiB），**此为估算，未验证** | — | Zig 最新稳定 0.17.1（2026-11-12 由 https://ziglang.org/download/index.json 读到版本号，但资产下载被阻断） |

### D. 静态 musl 的坑（全部为第一手验证或 musl 官方文档）

| 事实 | 数字/结论 | 来源 URL | 日期或版本 |
|---|---|---|---|
| 静态 musl 二进制在**完全空的 chroot**（无 /lib、无 /etc、无 /usr）中能否启动 | **能**。`chroot /tmp/emptyroot /tz_static` 正常输出，证明「单文件部署」成立 | 本机实测（附录 A.7） | musl-gcc 13.0.0 + WSL2 Debian |
| `getaddrinfo` 在静态 musl 里是否需要 NSS/dlopen | **不需要**。静态程序 `getaddrinfo("localhost","80")` 返回 0（成功）；musl 无 NSS 模块机制 | 本机实测（A.6）＋ https://musl.libc.org/about.html | 2026-11-12 |
| `dlopen` 在静态 musl 里的行为 | **能链接成功，运行时返回 NULL**（`printf("%p")` 打印 `0`，无崩溃、无报错）。静态 musl 内没有动态加载器 | 本机实测（A.7） | 2026-11-12 |
| 时区：静态二进制内是否内嵌 tzdata | **不内嵌**。空 chroot 内 `TZ=Asia/Shanghai` 静默退化为 UTC（hour 从 10 变 02，无任何警告）；宿主上同样二进制给出正确 +8 | 本机实测（A.7） | 2026-11-12 |
| 时区查找顺序 | `TZDIR` 优先，之后回落到 `/usr/share/zoneinfo` 等内置默认路径（实测把 `TZDIR` 指向不存在目录仍能取到正确时区，说明存在内置回落路径） | 本机实测（A.7） | 2026-11-12 |
| musl 的 DNS 解析器与 glibc 的差异 | 并行查询所有 nameserver（最多 3 个）而不是串行超时重试；`search`/`ndots` 语义与 glibc 不同（点数 ≥ ndots 时不再回落到 search）；DNS over TCP 自 1.2.4 才支持；**不支持 IDN** | https://wiki.musl-libc.org/functional-differences-from-glibc.html | 2026-11-12 查阅 |
| musl 的线程默认栈大小 | 128 KiB（1.1.21 之前 80 KiB），远小于 glibc 的 2–10 MB；可通过 `-Wl,-z,stack-size=N` 调整 | https://wiki.musl-libc.org/functional-differences-from-glibc.html | 2026-11-12 查阅 |
| musl 的 `dlclose` 与惰性绑定 | `dlclose` 是 no-op（库永不卸载）；不支持惰性绑定 | https://wiki.musl-libc.org/functional-differences-from-glibc.html | 2026-11-12 查阅 |
| musl 静态链接的常见崩溃诱因（官方 FAQ） | “assumptions that `pthread_create` will create sufficiently large stacks by default (crash situation)” | https://wiki.musl-libc.org/faq.html | 2026-11-12 查阅 |
| musl 的库布局 | 全部功能放进 `libc.a`/`libc.so`，`libm.a`/`libpthread.a` 等为空文件（仅为兼容存在） | https://wiki.musl-libc.org/faq.html | 2026-11-12 查阅 |

## 对技术选型的影响

**建议 1（确定性：高）：把「1 MB」从验收项的「单二进制」改为「每个二进制的预算表」，并按功能分层。**
理由：实测显示体积是被具体功能（HTTP 栈、TLS、JSON）线性拉动的，不是被语言常数拉动的。给 svcd 定 350–450 KiB、给 PM 定 500–600 KiB 都可达；给「两者合计 1 MB 且要 TLS」不可达。落法：在 CI 里对每个 release 产物做体积门禁（例如 `test $(stat -c%s bin) -le 460800`），超了就让 PR 失败，而不是事后发现。

**建议 2（确定性：高）：PluginManager 不要在进程内终结 TLS；把 TLS 交给外部 terminator。**
理由：rustls(ring) 一项就把 lean PM 从 496.8 KiB 抬到 1,180.7 KiB。若产品要求端到端 TLS 且不可外置，则必须放弃 1 MB 目标——这不是优化能解决的，是 ring 的 crypto 常量表和握手状态机体量决定的（实测 .rodata 单项 257,836 B）。落法：PM 只说 HTTP/1.1 明文（loopback 或 unix socket），由 systemd/launchd/SCM 侧或前置 nginx/caddy 做 TLS。

**建议 3（确定性：高）：Rust 是唯一同时满足「1 MB」与「可维护」的选项；Go 直接排除；TinyGo 排除。**
理由：Go 的 floor（hello 1.44 MiB strip 后）已经超预算，真实 PM 5.68 MiB，是 5–6 倍差距，且随版本上涨；TinyGo 官方不支持 `net/http`，而本项目核心就是内嵌 MCP Server + 反向代理。C 体积最优但手写 HTTP/JSON/进程监督的维护成本与内存安全风险不成比例。

**建议 4（确定性：中高）：Rust 内部按「std::net 手写 HTTP → tiny_http → hyper → axum」逐级评估，默认停在 tiny_http 或 hyper 之下。**
理由：相对 364.7 KiB 基准，手写 HTTP +32 KiB、tiny_http +196 KiB、hyper +280 KiB、axum +456 KiB。反向代理需要流式转发与 keep-alive，手写 HTTP 的风险偏高；**tiny_http 或 hyper + 手写路由**是体积/正确性的合理折中（约 560–645 KiB 单二进制）。避免 axum：456 KiB 的框架税在 1 MB 预算下不可接受。

**建议 5（确定性：中）：预算里必须留出「std 重建」这一步，并把 `-Z build-std` 纳入构建流水线。**
理由：`panic=abort` 单独用收益为 0（实测 0 字节），因为链接的是预编译 unwind 版 std；`build-std=std,panic_abort` 才拿到 7.7–8.9%。代价是需要 nightly + `rustup target add x86_64-unknown-linux-musl`，且实测在 Windows 宿主交叉编译时需 `-C link-self-contained=yes`。这是「为 8% 引入 nightly」的取舍——建议只在 release 通道用 nightly，dev 用 stable。

**建议 6（确定性：高）：插件只能是进程——这条已由静态链接本身强制，且与 1 MB 目标一致。**
理由：静态 musl 里 `dlopen` 链接通过但运行时返回 NULL，不存在「把插件做成 .so 动态加载」的选项。反过来这也帮了预算：PM 不需要链接任何插件 SDK。

**建议 7（确定性：高）：把「时区数据」和「DNS 配置」列为部署依赖，写进打包清单。**
理由：空 chroot 实测证明静态二进制自包含，但 `TZ=Asia/Shanghai` 会静默退化成 UTC——对日志时间戳是隐性故障。落法：要么在镜像里带 `/usr/share/zoneinfo`（OpenWrt 上 `zoneinfo-*` 是可选包），要么在代码里内嵌 tzdata（Rust `chrono-tz` 会显著增加体积，需重新评估预算），要么全系统统一 UTC 并在展示层转换。

**建议 8（确定性：中）：svcd 与 PM 应该在同一个二进制里，还是两个？按 1 MB 预算，两个更贵但更值。**
理由：两个 lean 二进制合计 921.5 KiB（build-std 后 ~843 KiB），而合并成一个会共享 std/libc/serde 的开销，估算能省 300–350 KiB（≈ 单进程 550–600 KiB）——**但这是估算，未实测**。省钱换来的是「PM 崩溃会带走监督者」以及必须靠 systemd/SCM 同时看护两个单元 vs 一个。既然 svcd 的设计前提就是「由外部 init 保活」，多一个单元的成本很低，建议保持两个二进制。

## 未确认项

1. **Zig 的实测体积**：本环境无法取得 Zig 工具链（`ziglang.org` 资产下载 0 字节，GitHub release 资产主机 `release-assets.githubusercontent.com` 连接被拒）。表中 Zig 行是估算，仅有 C 基准 13.0 KiB 作为锚点。若需要，请在可联网环境跑 `zig build-exe -target x86_64-linux-musl -O ReleaseSmall`。
2. **UPX 的收益与风险：完全未实测。** UPX 官方 release 资产同样被网络策略阻断，WSL 内 `apt` 也装不上。可以确定的只有算术：Go 的 hello（1.44 MiB）要在 1 MB 内需压缩 ≥30%，真实 PM（5.68 MiB）需 ≥82%——后者远超常规压缩比，因此**即使 UPX 有效也救不了 Go**。Rust lean PM（496.8 KiB）不需要 UPX。关于「UPX 压缩后更易触发杀软误报、破坏签名与静态分析、且不降低运行时内存（需整段解压）」这些风险说法，我**没有取得一手出处，故不作为结论**。
3. **`build-std` 对 svcd_lean 的具体数字**：只测了 hello（336.7 KiB）与 pm_lean（452.7 KiB），未单独重测 svcd_lean；按 −8% 推算约 390 KiB，属估算。
4. **「合并为单二进制能省 300–350 KiB」是估算**，未构造合并版本实测。
5. **glibc 静态链接下 `getaddrinfo` 的具体失败形态**：未取得 glibc 官方原文的报错文本，故本文只引 musl 官方对「glibc 的 DNS 依赖动态加载」的对照表述，不给出 glibc 侧的具体错误字符串。
6. **`clock_gettime`/`stat` 等在新内核上的 vDSO 行为**：未测，与体积无关但与守护进程定时精度有关。
7. **musl 版本**：本机 musl-gcc 为 GCC 13.0.0 的 2022-05-30 快照，未记录其内嵌 musl 版本号；这会影响 DNS over TCP 等特性是否可用（需 ≥1.2.4）。
8. **实际目标硬件上的 `.text` 之外开销**：仅测了文件体积，未测 RSS / 页缓存占用。嵌入式上 1 MB 文件体积与运行时内存是两件事，本文不覆盖后者。

## 附录 A：可复现步骤

### A.0 测量环境
- Windows 11 宿主 + WSL2 Debian（kernel 6.18.40.1-microsoft-standard-WSL2），用于**运行**验证静态二进制。
- rustc 1.99.1 (58cf8f9c3 2026-10-13) stable，target `x86_64-unknown-linux-musl`；rustc 1.102.0-nightly (ac8310ba6 2026-11-09) 用于 `build-std`。
- musl-gcc (GCC) 13.0.0 20220530（`x86_64-linux-musl` 交叉工具链）。
- Go 1.27.4 windows/amd64（`GOOS=linux CGO_ENABLED=0` 交叉编译）；对照用 Go 1.22.0。
- 体积一律取文件字节数（`(Get-Item $bin).Length`），换算按 1 KiB = 1024 B、1 MiB = 1048576 B。

### A.1 Rust 体积矩阵与各 crate 成本
统一 profile（Cargo.toml）：

```toml
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

```bash
cargo new --bin <name> && cd <name>
# 依赖用 cargo add：
#   svcd_lean:      libc
#   pm_lean:        libc, serde --features derive, serde_json
#   rs_tokio_cur:   tokio --features rt,net,signal,time,process,io-util,macros, libc
#   rs_axum:        axum, tokio --features rt-multi-thread,net,signal,macros, serde --features derive, serde_json
#   rs_hyper:       hyper --features http1,server, hyper-util --features tokio,server, http-body-util, tokio, bytes
#   rs_rustls:      rustls --no-default-features --features ring,std,tls12, tokio --features rt,net,io-util,macros
cargo build --release --target x86_64-unknown-linux-musl
```

标志矩阵（A.1 前 6 行）用自定义 profile 继承实现：`[profile.p_strip] inherits="release"`、`[profile.p_strip_abort]`、`[profile.p_lto]`、`[profile.p_z]`、`[profile.p_s]` 依次叠加 `strip` → `panic="abort"` → `lto+codegen-units` → `opt-level`。

### A.2 build-std
```bash
rustup +nightly target add x86_64-unknown-linux-musl
RUSTFLAGS="-C link-self-contained=yes" \
  cargo +nightly build --target x86_64-unknown-linux-musl \
    -Z build-std=std,panic_abort --profile release
```
（不加 `-C link-self-contained=yes` 会在 Windows 宿主上报 `cannot find crt1.o` / `cannot find -lunwind`。）

### A.3 段级拆解
```powershell
& (Join-Path (rustc --print sysroot) 'lib\rustlib\x86_64-pc-windows-msvc\bin\llvm-size.exe') -A <binary>
```

### A.4 静态性验证
```bash
ldd ./pm_lean        # -> "statically linked"
```

### A.5 Go
```bash
GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o hello_default .
GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o hello_stripped .
# pmsim 另加 arm64 / 386 两个 GOARCH 重复上述两条
```

### A.6 C
```bash
musl-gcc -static -Os -s -o hello_static hello.c
musl-gcc -static -Os -s -o daemon_static daemon.c   # sigaction + sleep loop
musl-gcc -static -Os -s -o ga_static ga.c           # getaddrinfo
```

### A.7 空 chroot 与运行时依赖验证
```bash
mkdir -p /tmp/emptyroot && cp ./tz_static /tmp/emptyroot/
TZ=Asia/Shanghai ./tz_static                      # hour=10（宿主有 tzdata）
chroot /tmp/emptyroot /tz_static                  # 能启动 => 单文件自包含
TZ=Asia/Shanghai chroot /tmp/emptyroot /tz_static # hour=02 => 静默退化为 UTC
./dltest_static                                   # 打印 0（NULL）=> dlopen 在静态 musl 下不可用
```
