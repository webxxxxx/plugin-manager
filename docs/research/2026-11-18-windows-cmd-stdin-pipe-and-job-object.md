# Windows：cmd 批处理就绪握手与 job 对象的两处未文档化行为

日期：2026-11-18。针对 #159（`enable_disable_round_trip_and_boot_skip` 偶发 `start timed out`）。

问题：把「测试假设（子进程 30 s 内没 ready）」和「实现丢 readiness 通知」分开，需要知道就绪握手依赖的
cmd/job 语义里哪些是文档保证的、哪些是没保证的。

## 1. `set /p` 从重定向 stdin 读一行

官方文档对 `/p` 只写「Sets the value of `<variable>` to a line of input entered by the user」。
对重定向文件/管道输入、EOF 时的 errorlevel、以及是否预读，均无说明。

- 结论：`set /p` 从管道读行**不是文档化契约**，用它当就绪信号等于依赖未文档化行为。
- 确定度：高（文档缺失本身是确定的）；实际行为：未文档化。
- 来源：https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/set_1

## 2. `cmd.exe /c *.cmd` 是否预读 stdin

没有找到文档说明 cmd.exe 在跑批处理时会（或不会）预读 stdin，也没有关于它与 `set /p` 先后关系的说明。

- 结论：未文档化，不能据此下判断。
- 旁证（本机实测，非文档）：同一批处理从进程启动到回包 22–36 ms（3 次），说明正常路径没有长阻塞，
  若失败现场是「子进程没回」，需要约 1000 倍的异常。
- 确定度：行为未文档化。

## 3. 句柄继承与 stdin EOF

通用 Win32 语义：`CreatePipe` 产生的句柄若可继承，会随 `CreateProcess` 传到子进程；父进程若另持一个
可继承的 stdin 写端，子进程就读不到 EOF。

- 本仓实现声明只让子进程端可继承（`src/stdio.rs` 模块注释），本轮**未逐行核对** std 的实现。
- 结论：这条要按「未文档化 + 未核对」处理，不作为判定依据。
- 确定度：低。

## 4. `TerminateJobObject` 之后复用同一个 job 句柄

文档保证：`TerminateJobObject` 终止当前关联的全部进程，且不可被进程推迟或处理；job 在「最后一个句柄
关闭且所有关联进程已终止」时销毁。文档**没有**说明：已终止的 job 是否接受后续
`AssignProcessToJobObject`，以及之后加入的进程是否会被立刻终止。

- 本仓相关：每个服务一个 job 句柄（`Tree`）只创建一次、跨 spawn 复用；`Tree::terminate()` 直接
  `TerminateJobObject`，不清空/重建句柄。
- 若「已终止 job 会立刻杀掉新加入进程」成立，那么**任何一次强制停止之后，该服务的后续 start 都会变成
  启动超时**。这是一条可以脱离 #159 单独验证的假设（一次强停 → 再 start 的单元测试即可）。
- 确定度：文档未说明（高）；实际行为：未知，待实测。
- 来源：https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
  https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-terminatejobobject

## 与本仓的关联（可观测证据）

- 「子进程没 ready」：握手基线 22–36 ms，需要约 1000 倍异常；子进程消失会留下 `stdout closed` 日志。
- 「实现丢通知」：需要 spawn 的 pid、ping 写入结果、每帧的采纳/丢弃原因；这三处现在都不打日志。
- 因此外部文档不足以定性 #159；先把上面两处观测点补上，再谈修哪一侧。
