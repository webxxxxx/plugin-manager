# 宽改：一次机械改动，几百个调用点

「宽改」指**形状**改动：给一个类型换错误类型、给一个符号改名、把一个模块搬到新目录。它的
特点是改法完全机械，但调用点散布在全仓，一处 edit 不可能全部覆盖，中间态又必须保持能编译。

判据：**如果每个改动点都长得一样，它是宽改；如果有些点要判断，它是行为改动，走 TDD。**

## 三步：expand → migrate → contract

1. **expand**：先把新形式加到旧形式旁边，让旧调用点继续编译。这一步单独一票、单独一个提交，
   可以随时停在这里。
2. **migrate**：按目录 / 按模块分批把调用点换过去，每批一个清单、一次 codemod、一次编译门。
   批次大小取「一次 review 能看完」的量，不要一次全仓。
3. **contract**：旧形式没有调用点之后删掉它。删除前 `grep` 一遍旧形式，把残留当成第一类错误处理。

每步结束时树都是绿的；不要把行为改动塞进任何一批（review 的 Spec 轴会把它算成 scope creep）。

## 工具：`tools/codemod.mjs`

清单驱动，精确匹配，门不过就还原：

```sh
node tools/codemod.mjs plan.json                # dry run，只报告
node tools/codemod.mjs plan.json --apply        # 写盘，然后跑 cargo check（默认门）
node tools/codemod.mjs plan.json --apply --verify "cargo check --target x86_64-unknown-linux-musl"
node tools/codemod.mjs --selftest               # 工具自身的 5 个用例
```

清单格式（`count` = 这一处 `old` 在**当前**文件里应当出现的次数，省略即 1）：

```json
{
  "edits": [
    { "file": "src/package.rs", "old": "Result<_, String>", "new": "Result<_, ProblemError>", "count": 66 },
    { "file": "src/plugin.rs", "old": "return Err(error);", "new": "return Err(error.into());", "count": 7 }
  ]
}
```

三条规矩，对应 #151 的两次翻车：

- **计数必须对上**。找不到、或者找到的次数多于/少于声明，整批拒写。盲 `replace` 之所以危险，
  是它把「计划与代码不一致」变成静默的半成品。
- **一批只做一件事**。类型改了就不再顺手动 `use`；下一次清单再改 import。混在一批里时，
  编译报错分不清是哪个变换漏了。
- **门必须在写盘之后立刻跑**。红了自动逐字节还原，现场不会留在 `git status` 里等人猜。

## 整块 item 搬家

「把这些 item 搬到新模块」也是宽改，但没有调用点要改，只有**文件边界**要动。手写提取脚本
翻车过两次：`#[cfg(unix)]` / `#[cfg(not(unix))]` 同名双实现被按名字覆盖；两个 item 之间的
`pub use` 块被一起吞进目标文件。用清单 + 门：

```sh
node tools/move-item.mjs plan.json            # dry run：列出 item 与行号
node tools/move-item.mjs plan.json --apply    # 写盘，然后 cargo check --all-targets
```

```json
{
  "moves": [
    {
      "from": "src/manage/package.rs",
      "to": "src/manage/model/ustar.rs",
      "header": "//! ustar 编解码。\n\nuse std::io::Write;\n",
      "items": ["TarEntry", "read_tar", "impl TarPlan"]
    }
  ]
}
```

- item 连同紧贴的 `///` 与 `#[...]` 一起搬；目标不存在时写入 `header`。
- 同名多处**直接拒绝**并列出全部行号——那通常就是 `#[cfg]` 双实现；确实要搬其中一半就写
  `名字@行号`。
- 名字可带 kind（`fn probe`）也可裸写（`TarEntry`；`impl` 要带 `impl ` 前缀）。
- 门红了逐字节还原两个文件并退出 1。

## 门顺序

```sh
node tools/codemod.mjs plan.json         # 连跑两遍，输出必须一致（幂等）
node tools/codemod.mjs plan.json --apply # 默认门：cargo check --all-targets
node tools/check.mjs cross               # Windows 宿主必须：cfg(unix) 那半默认不编译
node tools/check.mjs full                # 行为没动也应该绿；变了就是清单写歪了
```

`--skip-verify` 只用于「先看清写盘结果」的临时探路，落地前必须再跑一次带门的 `--apply`。

## 格式化

只对**点名的文件**跑：

```sh
node tools/fmt.mjs src/manage/package.rs
node tools/fmt.mjs --check src/manage/package.rs
```

直接 `rustfmt src/manage/mod.rs` 会顺着 `mod` 声明递归格式化整棵子树（一次卷进 27 个无关
文件）；`fmt.mjs` 带 `--config skip_children=true`，并且拒绝 `src/`、`tests/` 以外的路径。
没有「格式化全仓」模式：不传文件就是用法错误。

## 反例（#151 实录）

- 手写 `str.replace` 全局替换，一次把 `Result<T, String>` 改成了 `Result<T>`，66 个签名丢掉
  错误类型；一次多出一个右括号。两次都只能 `git checkout -- src/blob.rs` 重来。
- 每个脚本都临时现写一个「精确匹配一次否则拒绝」的守卫，同一段逻辑写了 16 遍。
- 脚本留在 worktree 根（`_blob2.py`、`_fix_contains.py`），靠人记得删；事后没人说得清哪个
  脚本跑过、跑了什么。
- 编译报红 28 次，每次都靠 `cargo check 2>&1 | grep -E "^error" -A5 | head` 手搓错误列表。

结论：**同一个机械动作被重复执行十几次时，把它写进 `tools/`，让下一次只写清单。**
