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
| 客户端上传超过 512 MiB | HTTP 413 |
| 客户端没有 blob 权限 | HTTP 401/403 |

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

- `src/blob.rs`：存储、id 校验、路径解析、结果物化。
- `src/api.rs`：`POST/GET /api/v1/blobs`。
- `src/slot.rs`：注入 `SVCD_TEMP_DIR`，解析 `$blob`，物化 `$blobPath`。
- `src/mcp.rs`：结果包装与 resource link。
