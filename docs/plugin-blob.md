# 插件大对象：`$blob`、`$blobPath` 与 `SVCD_TEMP_DIR`

> 面向插件作者和工具门户开发者。目标：让超过 1 MiB 的参数/结果不进入 JSON-RPC 控制帧，同时保持鉴权、路径边界和可审计性。

## 1. 它解决什么问题

插件控制通道（stdio JSON-RPC）的单帧上限是 1 MiB。视频、模型、文件处理类插件经常需要：

- 传入一个大文件作为参数；
- 返回一个大文件作为结果；
- 不在模型上下文里塞入二进制内容。

svcd 提供内容寻址的 blob store：

- 参数：客户端/门户上传 blob，工具参数只携带 `{"$blob":"sha256:<hex>"}`；
- 结果：插件把文件写到 `SVCD_TEMP_DIR/incoming/`，返回 `$blobPath`，宿主落成 blob 并返回 MCP `resource_link`。

控制帧里始终只有小 JSON。

## 2. 目录与配置

| 名称 | 含义 |
|---|---|
| `SVCD_TEMP_DIR` | 宿主通过环境变量传给插件的临时根目录 |
| `<SVCD_TEMP_DIR>/sha256-...` | 已落盘的内容寻址 blob（文件名是十六进制摘要） |
| `<SVCD_TEMP_DIR>/incoming/` | 插件写结果的暂存目录；宿主校验后移入 blob store |
| `sha256:<hex>` | blob id（64 位小写十六进制） |

路径示例（Linux）：

```text
/var/lib/svcd/temp/incoming/report-7f3a.txt
/var/lib/svcd/temp/9f2c...e1
```

Windows 使用对应的数据目录；插件应始终使用宿主注入的 `SVCD_TEMP_DIR`，不要硬编码。

## 3. 上传：给工具参数准备 blob

门户/客户端调用管理 API：

```http
POST /api/v1/blobs
Content-Type: application/octet-stream
Authorization: Bearer <password-or-session>
```

请求体是原始字节；响应：

```json
{"id":"sha256:9f2c...e1","size":10485760}
```

下载：

```http
GET /api/v1/blobs/sha256:9f2c...e1
```

需要与管理 API 相同的鉴权。单 blob 上限 **512 MiB**。

### HTTP 上传

`POST /api/v1/blobs` 接受以下主流写法，上传内容边读边写 `temp_dir`，同时增量计算 SHA-256，daemon 内存不随文件大小增长：

- 原始字节：`Content-Type: application/octet-stream`（默认；其它非 multipart 类型也按原始字节处理）；
- 浏览器表单：`Content-Type: multipart/form-data; boundary=...`，取第一个带 `filename=` 的文件字段；
- 长度：`Content-Length` 或 `Transfer-Encoding: chunked` 均可；
- `Expect: 100-continue`：HTTP/1.1 客户端可以在发送大 body 前等待 `100 Continue`；
- 单 blob 上限 **512 MiB**；超过返回 413（`Content-Length` 在读取前判断，chunked 在读取过程中判断）；
- daemon 同时最多处理 `max_streams` 个 blob 上传/下载流（默认 64），超出返回 503 `{"error":{"code":"busy"}}` + `Retry-After: 1`；上传的 body 经阻塞池写入 staging，慢客户端只会让本连接暂停读取。

上传成功返回 200：

```json
{"id":"sha256:9f2c...e1","size":10485760}
```

响应带 `ETag: "sha256:9f2c...e1"` 与 `Location: /api/v1/blobs/sha256:9f2c...e1`。

### HTTP 下载

`GET /api/v1/blobs/{id}` 经 sans-IO stream handler 从磁盘流式返回（优先内核 sendfile，否则受 stream buffer 限制的分块读写）；`HEAD` 只返回头。响应头：

- `Content-Type: application/octet-stream`
- `Content-Length`
- `Accept-Ranges: bytes`
- `ETag: "sha256:<hex>"`（强校验器；内容寻址，内容不变 id 不变）
- `Last-Modified`
- `Cache-Control: no-cache`

#### Range 与条件请求

- `Range: bytes=start-end`、`bytes=start-`、`bytes=-suffix` → 206 + `Content-Range: bytes start-end/total`；
- 支持单段 Range；多段 Range 按整段 200 返回（不生成 `multipart/byteranges`）；
- 不可满足的 Range → 416 + `Content-Range: bytes */total`；
- `If-None-Match` / `If-Modified-Since` 命中 → 304；
- `If-Match` / `If-Unmodified-Since` 不命中 → 412；
- `If-Range` 的校验器不匹配时忽略 Range，返回 200 整段。

### 主动释放：`DELETE`

传输产物默认按 `temp_retention_secs`（默认 7200 秒）过期清理；需要立刻腾出空间时用 DELETE 显式释放，不等清理周期：

```http
DELETE /api/v1/blobs/sha256:9f2c...e1
DELETE /api/v1/file/<id>
```

- 成功 `200 {"removed":true}`；同一个 id 第二次删除返回 `404`（没有东西可删不是错误，也不隐藏答案）；
- `/api/v1/file/{id}` 删的是整包：清单与它的分片同属一个单元；
- 插件商店装包走的就是这条路：浏览器 `GET /api/v1/blobs/{id}` 拿到字节后立刻 DELETE，**安装期的磁盘峰值是一份包，不是两份**；安装入口自己也会在成功/失败后清掉 `temp` 里的源包（见 §4 的 `plugin.install`）。空间仍然不够时（单卷峰值 > 空闲），只有先卸旧版再装新版这一条路，导出备份是流式的、不占设备磁盘。
- 装之前问一句"装得下吗"用 `GET /api/v1/disk[?id=<插件 id>]`：`roots` 列表给出每个插件根的空闲字节、哪一个是默认根、这个 id 是否已经装在该根上（`plugin.bytes` 是旧版占用，用来决定原地升级还是先卸载），`tempFreeBytes` 是同一次安装会经过的临时卷。

### 断点续传 / 分片上传 / 并行上传

一次整体上传适合几十 MB 到 512 MiB 的文件；需要断点续传、重传失败分片或并行上传时，使用 S3 风格的 part 会话（JSON 变体）。所有分片先落在 `temp_dir/uploads/<uploadId>/`，`complete` 时按顺序拼装并计算最终 SHA-256。

会话在创建时记录类型：`/api/v1/blobs/uploads` 创建的是 **blob 会话**，只能用 blob 的 part/status/complete/abort；`/api/v1/file/uploads` 创建的是 **file 会话**，只能用 file 侧接口。用错一侧返回 `409 Conflict`，不会误把文件包拼成 blob 或反过来。

1. 创建会话：

```http
POST /api/v1/blobs/uploads
Authorization: Bearer <password-or-session>
```

```json
{"uploadId":"<32 hex>","partsUrl":"/api/v1/blobs/uploads/<uploadId>/parts"}
```

2. 上传一个分片（`partNumber` 1..10000）：

```http
PUT /api/v1/blobs/uploads/<uploadId>/parts/1
Content-Type: application/octet-stream
Content-Length: 67108864
```

分片 body 同样支持 `Transfer-Encoding: chunked`。返回：

```json
{"partNumber":1,"size":67108864,"etag":"sha256:..."}
```

同一个 `partNumber` 可以重复 PUT，后一次覆盖前一次，因此失败分片直接重传即可；不同 `partNumber` 可以并行上传。

3. 查询已完成分片（断点续传入口）：

```http
GET /api/v1/blobs/uploads/<uploadId>
```

```json
{"uploadId":"...","totalSize":134217728,"parts":[{"partNumber":1,"size":67108864,"etag":"sha256:..."}]}
```

4. 完成拼装：

```http
POST /api/v1/blobs/uploads/<uploadId>/complete
Content-Type: application/json

{"parts":[{"partNumber":1,"etag":"sha256:..."},{"partNumber":2,"etag":"sha256:..."}]}
```

`parts` 必须按 `partNumber` 严格递增；`etag` 可省略，但建议带上以便服务端校验。body 为空时按现存全部分片升序拼装。成功返回与整体上传相同的 `{"id":"sha256:...","size":...}`，并删除会话。未列出的分片会被丢弃。

5. 中止会话：

```http
DELETE /api/v1/blobs/uploads/<uploadId>
```

会话和分片遵循 `temp_dir` 的 TTL（默认 2 小时，每 5 分钟扫描）。空会话有标记文件，不会被当作空目录立即删除；超过 TTL 未活动仍会被清理。

### 大文件：`/api/v1/file` 文件包

单 blob 上限仍是 **512 MiB**；更大的文件不再塞进一个 blob，而是做成「文件包」：多个 part + 一个 manifest，文件 id 是整个文件的 SHA-256。每个 part 仍受 512 MiB 限制，`complete` 只校验并哈希 part，不会把 part 再复制成一个大文件。

```text
POST   /api/v1/file/uploads                    创建文件上传会话
PUT    /api/v1/file/uploads/{id}/parts/{n}     上传 part（≤ max_file_part_bytes = 512 MiB）
GET    /api/v1/file/uploads/{id}               查询已完成 part
POST   /api/v1/file/uploads/{id}/complete      校验/哈希并提交为文件包
DELETE /api/v1/file/uploads/{id}               中止

GET    /api/v1/file/{sha256:...}               流式下载，支持 Range/HEAD/条件请求
HEAD   /api/v1/file/{sha256:...}
```

`/api/v1/file/uploads` 创建的是 **file 会话**；它的 part/status/complete/abort 只能走 `/api/v1/file/...`。拿同一个 id 去 `/api/v1/blobs/uploads/...` 的 part/status/complete/abort 会返回 `409 Conflict`，反过来也一样。

`complete` 的 body（`parts` 可省略，表示全部现存 part 按序）：

```json
{
  "name": "proxmox-ve_9.2-1.iso",
  "mimeType": "application/x-iso9660-image",
  "parts": [
    {"partNumber": 1, "etag": "sha256:..."},
    {"partNumber": 2, "etag": "sha256:..."}
  ]
}
```

返回：

```json
{"id":"sha256:4e88...","size":1706178560,"name":"...","mimeType":"..."}
```

文件包存放在 `temp_dir/files/<sha256>/`：`manifest.json` + `part-00001` 等。文件 id 是整文件 SHA-256；同样的内容重复提交会复用已有文件包（整文件级去重）。当前 `max_file_bytes` 为 16 GiB，part 数上限 10000；清理按 `manifest.json` 的 mtime 整体删除，下载会 touch manifest。

#### 工具参数里的 `$file`

大文件不要用 `$blob`（它表示单个 blob）。工具调用可以传：

```json
{
  "input": {"$file": "sha256:4e88..."}
}
```

宿主解析为：

```json
{
  "$file": "sha256:4e88...",
  "dir": "/var/lib/svcd/temp/files/4e88...",
  "manifest": "/var/lib/svcd/temp/files/4e88.../manifest.json",
  "size": 1706178560,
  "name": "proxmox-ve_9.2-1.iso",
  "mimeType": "application/x-iso9660-image"
}
```

插件读取 `manifest.json` 的 `parts`，按 `partNumber` 顺序读 `part-<n>` 文件；不愿读目录的插件也可以直接走 `GET /api/v1/file/{id}` 流式下载。`$blob` 的旧语义完全不变。

### 零拷贝与内核辅助 I/O

当前实现会在平台支持时尽量把数据留在内核/页缓存里，所有路径都有普通读写的 fallback：

- **上传**：仍走普通读/写。评估过预分配 + `mmap` 窗口，在 Windows/loopback 基准里页错误、预分配和刷新成本高于省下的 memcpy，且映射页会抬高 RSS，因此没有启用。
- **chunked / multipart 解码**：分块数据直接以输入切片交给 sink，只缓存不完整的大小行/边界尾部，不再把整个 chunk 复制进解析器 buffer。
- **下载（≥64 KiB）**：Linux 用 `sendfile`，Windows 用 `TransmitFile`，由内核把文件页送到 socket；文件包按 part 逐段发送，Range 用 part 内偏移。小块和平台不支持时回退到 64 KiB 读/写。基准里 daemon CPU 从约 4.4 核降到约 0，但 Windows loopback 墙钟略慢，属于用吞吐换 CPU 的取舍。
- **`$blobPath` 结果导入**：在 `incoming/` 原地算 SHA-256，然后 `rename` 到 blob store；不再复制出第二份文件。
- **文件包/`import_file` 哈希**：仍按 64 KiB 顺序读。只读 `mmap` 窗口能省一次拷贝，但实测会把并发完成的 daemon RSS 从 8.6 MiB 抬高到 15 MiB 以上，收益不值得，因此没有启用。

这些优化的边界：

- `sendfile`/`TransmitFile` 只适用于明文 socket；将来加 TLS 后需要回退到读/写。
- 小块仍然走读/写，避免零拷贝 API 的固定开销。
- 建议插件作者对只读扫描/哈希使用 `mmap`，对转发文件使用 `sendfile`/`copy_file_range`，不要把大文件整体读进内存。

### 示例（curl）

整体上传（原始字节）：

```bash
curl -X POST --data-binary @video.mp4 \
  -H "Authorization: Bearer <password>" \
  -H "Content-Type: application/octet-stream" \
  http://127.0.0.1:8787/api/v1/blobs
```

chunked 上传：

```bash
curl -X POST --http1.1 -H "Transfer-Encoding: chunked" \
  -H "Authorization: Bearer <password>" \
  --data-binary @video.mp4 \
  http://127.0.0.1:8787/api/v1/blobs
```

表单上传：

```bash
curl -F "file=@video.mp4" \
  -H "Authorization: Bearer <password>" \
  http://127.0.0.1:8787/api/v1/blobs
```

分段下载：

```bash
curl -H "Authorization: Bearer <password>" \
  -H "Range: bytes=0-1048575" \
  -o first-mib.bin \
  http://127.0.0.1:8787/api/v1/blobs/sha256:<hex>
```

断点续传（示意）：

```bash
TOKEN=<password>
UPLOAD=$(curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8787/api/v1/blobs/uploads | jq -r .uploadId)
split -b 64m video.mp4 part-
curl -X PUT --data-binary @part-aa \
  -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8787/api/v1/blobs/uploads/$UPLOAD/parts/1
# 失败/中断后查询已完成分片：
curl -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8787/api/v1/blobs/uploads/$UPLOAD
# 完成：
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"parts":[{"partNumber":1,"etag":"sha256:<part1-etag>"}]}' \
  http://127.0.0.1:8787/api/v1/blobs/uploads/$UPLOAD/complete
```

宿主有一个后台清理线程：默认删除 `temp_dir` 下 **超过 2 小时**未访问的文件，每 5 分钟扫描一次。配置项：

```ini
# 不写时默认 <data>/temp
temp_dir = /var/tmp/svcd
# 默认 7200 秒（2 小时）
temp_retention_secs = 7200
# 默认 300 秒；设为 0 可完全关闭清理
temp_cleanup_interval_secs = 300
```

`SVCD_TEMP_DIR` 环境变量优先于 `svcd.conf` 的 `temp_dir`，主要用于测试和隔离运行。

安装包不算「未访问文件」：`plugin.install` 的来源落在 `temp_dir` 里时（blob、`files/<id>` 文件包或 `incoming/` 里的一个文件），**安装成功和失败都会立刻删掉这份来源**（#286）。内容寻址存储的语义是「用完即弃的输入」，安装后的版本目录才是要留下的东西；这样小容量卷不会因为一次失败的 15 MiB 安装被占到 TTL 到期。

自升级也是同一个语义：`plugin-store.release_asset` 每 `download=true` 一次就物化一个 blob（归档、`SHA256SUMS` 各一个），宿主在 `selfinstall::upgrade` 里把它收成一次性交接（`Handover`），**校验失败、`tar` 解包失败、目标不可写、安装成功**都在离开作用域时立刻删掉这些条目（#302）。

设备上的 `tar` 常是 BusyBox 版本，它不做压缩自动识别：`.tar.gz` 必须显式带 `-z`（`-xzf`），否则读到的就是「`invalid tar magic`」，跟「没装 tar」表现不同但同样解不开。因此解包按**内容头**（gzip magic `1f 8b`）选 `-xzf`/`-xf`，与文件名无关（`svcd upgrade --file` 接受任意本地归档）；解包失败时 `tar` 自己的 stderr 会拼进错误文本，控制台不必再让人 SSH 上去看。

> 升级提示：旧版本默认目录是 `<data>/blobs`，改名为 `<data>/temp` 后旧文件不会自动迁移。需要保留旧 blob 时，可以手工把目录改名为 `temp`，或在 `svcd.conf` 里把 `temp_dir` 指回旧目录（插件环境变量仍会变成 `SVCD_TEMP_DIR`）。

## 4. 参数：`$blob`

工具调用参数里可以放：

```json
{
  "input": {"$blob": "sha256:9f2c...e1"},
  "prompt": "生成 10 秒视频"
}
```

宿主在把参数转发给插件前，会把该对象替换为：

```json
{
  "$blob": "sha256:9f2c...e1",
  "path": "/var/lib/svcd/temp/9f2c...e1",
  "size": 10485760
}
```

插件应：

1. 读取 `arguments.<字段>.path`；
2. 把它当**只读文件**打开；
3. 需要流式处理时按文件读，不要整个读进内存；
4. 不要假设这个路径会长期存在：宿主默认按 2 小时 TTL 清理，长时间读取需要业务方重新解析参数或提高 `temp_retention_secs`。

缺失/非法 blob 会让宿主在调用插件前返回工具错误。

## 5. 结果：`$blobPath`

插件生成大结果时：

1. 写入 `SVCD_TEMP_DIR/incoming/` 下的唯一文件名；
2. 返回一个**小对象**：

```json
{
  "$blobPath": "/var/lib/svcd/temp/incoming/report-7f3a.txt",
  "name": "report.txt",
  "mimeType": "text/plain"
}
```

要求：

- `$blobPath` 必须是 `incoming/` 内的绝对路径（宿主会 canonicalize，拒绝逃逸和符号链接逃逸）；
- 文件大小 ≤ 512 MiB；
- 文件名唯一，避免并发覆盖；
- 写入完成后 flush/close，再返回结果。

宿主会：

1. 校验路径在 `incoming/` 内；
2. 流式读取并计算 SHA-256；
3. 落成内容寻址 blob；
4. 删除 incoming 源文件；
5. 把结果替换为：

```json
{
  "content": [
    {"type":"text","text":"blob sha256:..."},
    {
      "type":"resource_link",
      "uri":"svcd://blob/sha256:...",
      "name":"report.txt",
      "mimeType":"text/plain"
    }
  ],
  "structuredContent": {"$blob":"sha256:...","size":12345},
  "isError": false
}
```

MCP 客户端/门户可以用 `structuredContent.$blob` 或 `resource_link` 中的 id 再走 `GET /api/v1/blobs/{id}` 下载。`svcd://` 是宿主自定义 scheme；不能直接解析该 scheme 的客户端应使用 HTTP 下载接口。

## 6. 失败语义

| 情况 | 行为 |
|---|---|
| `$blob` id 不存在 | 宿主在调用插件前返回工具错误 |
| `$blobPath` 不在 `incoming/` | 宿主返回工具错误，不落盘 |
| 结果文件超过 512 MiB | 宿主返回工具错误，删除临时文件（尽力） |
| 写入后哈希失败/IO 错误 | 工具错误；残留文件由后台清理线程按 TTL 删除 |
| 客户端上传超过 512 MiB | HTTP 413（chunked 在读取中判断并删除临时文件） |
| 文件包某个 part 超过 512 MiB | HTTP 413（`max_file_part_bytes`） |
| 文件包总大小超过 16 GiB | HTTP 413（`max_file_bytes`） |
| 客户端没有 blob 权限 | HTTP 401/403 |
| 下载 Range 不可满足 | HTTP 416 + `Content-Range: bytes */total` |
| 条件请求 `If-Match` / `If-Unmodified-Since` 不满足 | HTTP 412 |
| 条件请求 `If-None-Match` / `If-Modified-Since` 命中 | HTTP 304 |
| 未知 blob id / uploadId | HTTP 404 |
| 会话类型与接口不匹配（blob/file 混用） | HTTP 409 |
| 非法 uploadId / partNumber | HTTP 400 |
| 同时带 `Content-Length` 与 `Transfer-Encoding` | HTTP 400 |
| 并发 blob 上传/下载超过 `max_streams` | HTTP 503 `busy` + `Retry-After: 1` |
| 并发下载超过 `max_streams` | HTTP 503 `busy` + `Retry-After: 1` |

插件应把 `$blobPath` 失败视为普通工具错误返回给调用者，不要伪造一个不存在的 blob id。

## 7. 示例：Python 插件

```python
import json, os, sys, uuid

temp_dir = os.environ["SVCD_TEMP_DIR"]

for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("method") == "svcd.ping":
        print(json.dumps({"jsonrpc":"2.0","id":msg["id"],"result":{"ok":True}}), flush=True)
        continue
    if msg.get("method") != "tools/call":
        continue

    args = msg["params"].get("arguments", {})
    input_path = args.get("input", {}).get("path")

    # 大结果先写 incoming，再返回引用。
    incoming = os.path.join(temp_dir, "incoming")
    os.makedirs(incoming, exist_ok=True)
    name = f"result-{uuid.uuid4().hex}.bin"
    path = os.path.join(incoming, name)
    with open(path, "wb") as out, open(input_path, "rb") as source:
        while chunk := source.read(1024 * 1024):
            out.write(chunk)

    print(json.dumps({
        "jsonrpc":"2.0",
        "id": msg["id"],
        "result": {
            "$blobPath": path,
            "name": "result.bin",
            "mimeType": "application/octet-stream"
        }
    }), flush=True)
```

## 8. 安全边界

- 客户端传来的 `$blob` 只是 id，不是路径；宿主负责解析，插件永远不该信任客户端给的任意路径。
- 插件返回的 `$blobPath` 会被 canonicalize 并限制在 `incoming/` 内；不要尝试 `..`、符号链接或指向其他目录。
- blob store 不是租户隔离的完整方案；多租户部署需要在网关/门户层做 blob 访问授权。
- 不要把密钥、token 写进 blob 文件名或内容；blob 内容不加密。
- 后台清理线程默认删除超过 2 小时的文件；仍在使用的 blob 在解析/读取时会被 touch，避免刚被引用就被删。长于 TTL 的持续读取仍需要业务方自行保活或提高 `temp_retention_secs`。

## 9. 与协议版本的关系

- 参数侧 `$blob` 对 `protocol: 1` 和 `protocol: 2` 都有效；宿主替换参数后，插件只看到普通 JSON。
- 结果侧 `$blobPath` 同样与插件协议版本无关；宿主在 `complete_pending` 阶段物化结果。
- 大结果**不要**尝试塞进单个 1 MiB 帧；即使协议 v2 也受 `CALL_FRAME_MAX` 限制。

## 10. 相关代码

- `src/blob.rs`：存储、id 校验、路径解析、流式写入、分片会话、文件包 manifest/哈希与结果物化。
- `src/blob_http.rs`：`/api/v1/blobs` 与 `/api/v1/file` 的流式上传/下载、Range/ETag/条件请求、multipart 与分片路由。
- `src/zerocopy.rs`：下载 `sendfile`/`TransmitFile` 与平台 fallback。
- `src/http.rs`：请求头解析、`Content-Length`/chunked framing、共享的 body 流式复制、HTTP 日期。
- `src/api.rs`：`POST/GET /api/v1/blobs` 路由、鉴权与普通管理 API。
- `src/slot.rs`：注入 `SVCD_TEMP_DIR`，解析 `$blob`，物化 `$blobPath`。
- `src/mcp.rs`：结果包装与 resource link。
