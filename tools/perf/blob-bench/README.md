# blob-bench（手动 Windows 基准）

`tools/perf/blob-bench/` 是 blob 与逻辑文件传输的手动 Windows 基准，属于 `tools/perf`
的一部分，但**不进** `run.mjs` 或 `perf.yml`：它需要一个 1.59 GiB 的 ISO、PowerShell 7
和一个 release 二进制。

## 内容

- `run-bench.ps1`：直接 `POST /api/v1/blobs` 上传 + `GET /api/v1/blobs/{id}` 下载的 ISO 并发基准。
- `run-file-bench.ps1`：`/api/v1/file` 逻辑文件（多 part）并发上传/下载 + filebench 插件读取基准。
- `upload-file-worker.ps1`：`run-file-bench.ps1` 的并发上传 worker，必须放在同一目录。
- `filebench.spk`：`run-file-bench.ps1` 安装的 filebench 插件。
- `sample-2026.pub`：与 `filebench.spk` 签名匹配的测试公钥（私钥不入库，仅用于验证）。
- `summary.md` / `summary-file.md`：2026-11-15 的实测结果与复现条件。

## 用法

要求 Windows + PowerShell 7（`pwsh`）、`curl.exe`、一个 release 版 `svcd.exe`
（在仓库里先 `cargo build --release`）。

```powershell
# file 逻辑文件基准（当前生产二进制即可）
pwsh -File tools/perf/blob-bench/run-file-bench.ps1 -Iso D:\path\to\image.iso

# blob ISO 基准：需要把 MAX_BLOB_BYTES 放宽到 > 1.59 GiB 的二进制
pwsh -File tools/perf/blob-bench/run-bench.ps1 -Iso D:\path\to\image.iso -Binary D:\path\to\svcd.exe
```

默认路径：

- `-Repo`：脚本向上三级，也就是仓库根。
- `-Binary`：`<repo>/target/release/svcd.exe`。
- `-Spk`：脚本同目录的 `filebench.spk`。
- `-WorkDir`：`%TEMP%/svcd-blob-bench/run` / `run-file`，不写回仓库。

脚本开头的默认 ISO 路径是原机器的 `D:\learn\pve\...`；换机器时用 `-Iso` 覆盖。

## 1.59 GiB ISO 的限制

生产 `MAX_BLOB_BYTES` 是 512 MiB，`run-bench.ps1` 的 ISO 会被 413 拒绝。复现时临时把
`src/blob.rs` 的 `MAX_BLOB_BYTES` 调大后 `cargo build --release`，用 `-Binary` 指向
那个二进制；**不要提交这个放宽**。`run-file-bench.ps1` 走 `/api/v1/file`，当前生产
上限 16 GiB，用默认 release 二进制即可。

## 历史

这些脚本原先散在 `target/blob-bench/`（会被 `cargo clean` 清掉）。2026-11-17 收进这里；
旧的完整源码副本（把 `MAX_BLOB_BYTES` 临时改到 4 GiB 的快照）没有进仓库。