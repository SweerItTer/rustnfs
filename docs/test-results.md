# RustNFS 测试记录

## 0.1.2：端口设置、恢复默认值与冲突提示

复测日期：2026-10-21。发布程序为 Windows x64 GNU release 构建，使用 Rust 1.99.1。
Windows EXE 和原生 Win32 界面在 **Wine 9 / Xvfb** 中实际运行，**不是实体 Windows 验证**。

| 本轮验证 | 实际结果 | 原始记录 |
| --- | --- | --- |
| Linux 传输层回归 | **9 / 9 通过**；新增 Portmapper / MOUNT / NFS 各自 TCP、UDP 的六种占用情形，检查错误中的具体端点及失败后的端口释放 | [传输层报告](test-reports/v0.1.2-transport.json) |
| Windows 服务 EXE：占用一个自定义 NFS UDP 端口 | **通过**；退出 1，日志包含具体 NFS UDP 地址、端口和原始错误 10048，不发送 READY，此前绑定的五个 TCP / UDP 端口全部释放 | [Windows 端口冲突报告](test-reports/v0.1.2-windows-port-conflict.json) |
| Windows GUI 实际交互 | **13 / 13 通过**；加载、编辑、保存重开、仅恢复端口、输入校验、命令预览与剪贴板、运行锁定、停止解锁、最小窗口和无效输入退出选择 | [GUI 报告](test-reports/v0.1.2-gui-ports.json) |
| Windows CLI / GUI release 构建 | **通过**；GUI 及后端测试使用的 EXE SHA256 与发布构建一致 | [构建及 SHA256](test-reports/v0.1.2-build.json) |

GUI 验证实际启动同目录下的后端，并分别向修改后的三个服务端口发送 TCP、UDP RPC NULL 请求，
确认六个监听均可响应；停止后检查端口释放。恢复默认端口后，共享目录、客户端列表、监听地址
及其他配置保持不变。空值、0、65536、非数字和重复端口均被拒绝，原配置文件不被覆盖。
端口尚未保存时，命令预览及 TCP / UDP 剪贴板内容已使用编辑值。

截图：[连接设置](screenshots/v0.1.2-ports-loaded.png)、
[运行状态](screenshots/v0.1.2-ports-running.png)、
[最小窗口](screenshots/v0.1.2-ports-minimum-size.png)。
诊断脚本位于 `tests/diagnostics/gui_ports_smoke.py`、`windows_gui_driver.c` 和
`windows_port_conflict.py`；所有测试仅使用隔离临时目录和本机回环端口，测试服务已停止。

本轮针对端口设置和启动错误路径验证，没有重跑完整 NFS 文件操作、第三方 libnfs、
Windows 防火墙或开发板测试。下方保留此前协议与文件系统的实际结果及已知 Wine 限制。

## 0.1.1：Windows 目录初始化修复

复测日期：2026-10-21。发布程序为 Windows x64 GNU release 构建，使用 Rust 1.99.1。
本轮 Windows 程序仍运行于 Linux 上的 Wine 9.0，**不是实体 Windows 验证**。

0.1.0 对目录调用了 `ReOpenFile`，真实 Windows 会拒绝这种调用并返回
`ERROR_ACCESS_DENIED`，随后被转换为缺少上下文的 `NFS3ERR_ACCES`。
[Wine 10.12 的兼容性测试](https://github.com/wine-mirror/wine/blob/wine-10.12/dlls/kernel32/tests/file.c)
明确覆盖这一行为；[官方发布记录](https://list.winehq.org/hyperkitty/list/wine-releases@list.winehq.org/2025/7/)
列出了目录测试与 `FILE_NON_DIRECTORY_FILE` 修正。此前 Wine 9 错误地允许该调用，掩盖了程序缺陷。
因此旧版在 Wine 中能启动不能证明旧版可以在实体 Windows 启动。

0.1.1 用 `GetFinalPathNameByHandleW` 获取已打开目录的路径，再以 `CreateFileW` 语义创建
只读属性的句柄，核对目录类型、reparse 属性和对象身份后才释放临时目录句柄。
实际路径遍历仍保留 cap-std 的保护；启动错误现在保留共享名称、路径、失败步骤及底层 OS 错误。

| 本轮验证 | 实际结果 | 原始记录 |
| --- | --- | --- |
| 用户完整配置：`C:\nfs`、`10.0.0.1/8`、端口 111/20048/2049，仅执行 `--check` | **退出 0，配置有效：1 个共享目录**；未监听网络端口 | [启动检查](test-reports/v0.1.1-windows-startup.json) |
| Linux 后端，独立 TCP / UDP 协议测试 | **35 / 35 通过** | [Linux 协议报告](test-reports/v0.1.1-linux-protocol.json) |
| Windows EXE / Wine 9，独立 TCP / UDP 协议测试及大小写改名 | **35 / 37 通过，2 项失败**；仍为下文记录的覆盖已有空目录行为 | [Windows 协议报告](test-reports/v0.1.1-windows-protocol.json) |
| Windows CLI / GUI release 构建 | **通过**；服务 EXE 已无 `ReOpenFile` 导入 | [构建及 SHA256](test-reports/v0.1.1-build.json) |

网络回归使用隔离临时目录、`127.0.0.1` 和高位端口；两个测试服务均已正常停止。
本轮没有重跑 GUI 交互、第三方 libnfs、防火墙、实体 Windows 或开发板测试。
下面保留 0.1.0 的原始报告与当时的测试范围，不能将其视为本版新增的实测结论。

## 0.1.0 历史记录

测试日期：2026-10-20。所有网络测试均使用隔离的临时共享目录和 `127.0.0.1` 高位端口。
原软件未参与这些测试。这里保留实际测试结果，不把兼容层限制计为通过。

## 结果

| 测试 | 结果 | 依据 |
| --- | --- | --- |
| Linux 后端，独立 Python NFSv3 / TCP / UDP 协议客户端 | **35 / 35 通过** | [原始 JSON](test-reports/linux-protocol.json) |
| Windows x64 EXE，Wine 9，增加大小写改名检查 | **35 / 37 通过，2 项失败** | [原始 JSON](test-reports/windows-protocol.json)；原因见下文 |
| 第三方 libnfs 5.0.2 → Linux 后端 | **通过** | [原始 JSON](test-reports/linux-libnfs.json) |
| 第三方 libnfs 5.0.2 → Windows EXE / Wine | **通过** | [原始 JSON](test-reports/windows-libnfs.json) |
| Windows GUI → 同目录真实 Windows 服务程序，Wine / Xvfb | **通过** | [原始 JSON](test-reports/gui-check.json) |

这是 Windows EXE 在 Wine 中执行的验证，并非实体 Windows、Windows 防火墙、Linux 内核挂载或开发板实测。
CLI、GUI 由同一份 Rust 源码构建；发布包二进制的 SHA256 见根目录 `SHA256SUMS.txt`。

## 协议测试覆盖了什么

`tests/protocol_smoke.py` 只使用 Python 标准库，自己编码 RPC/XDR，不导入服务端协议库。
它实际访问 Portmapper、MOUNT 和 NFS 服务，而非只检查程序是否能启动。

- TCP / UDP 端口发现、多个导出、合法与非法挂载路径。
- 195,334 字节二进制数据写入、读取及 SHA256 对比；COMMIT、三种 WRITE stable 请求、截断和 EOF。
- 普通文件和目录创建、中文文件名、空文件、分页目录读取及属性查询。
- GUARDED 创建冲突、明确返回不支持的 EXCLUSIVE 创建。
- 文件和父目录改名后使用原文件句柄；删除后同名重建时旧句柄失效。
- 持有子目录、孙目录和文件句柄时的目录改名/删除；Windows 大小写变体和仅改变大小写的改名。
- CREATE / MKDIR 拒绝无效 UID/GID、时间纳秒或文件大小时，不留下新的文件/目录。
- 只读共享拒绝创建、写入、修改属性和目录变更；ACCESS 与只读状态一致。
- 不合法文件名、越界路径、错误 RPC 版本、畸形 XDR、超大消息、TCP 分片/合并消息和 UDP 重放。

详细的分组结果、输入规模和返回值保留在原始 JSON 内。测试没有替代完整协议认证、长期压力测试或断电测试。

## Wine 中的两个失败分组

两个分组分别使用 TCP 和 UDP，遇到的是同一个操作：将一个非空源目录改名并覆盖已经存在的空目标目录。
服务返回 `NFS3ERR_ACCES`，没有删除源目录或目标目录。普通的目录改名与大小写改名已经通过。

为区分服务端逻辑和运行平台的行为，使用一个独立 Windows 小程序做了对照，源码见
[`tests/diagnostics/windows_rename_probe.rs`](../tests/diagnostics/windows_rename_probe.rs)，输出见
[原始记录](test-reports/windows-rename-probe.txt)：

| 不经过 NFS 的操作 | Wine 9 的实际结果 |
| --- | --- |
| `std::fs::rename`，目标不存在 | 成功 |
| 同上，同时持有目录及文件句柄 | 成功 |
| `std::fs::rename`，覆盖已有空目录 | Win32 错误 5：ACCESS_DENIED |
| 同上，同时持有目录及文件句柄 | 同样返回错误 5 |
| 直接调用 `SetFileInformationByHandle`，`FileRenameInfo`（3） | 成功，作为结构和参数的正向对照 |
| 相同方式调用 `FileRenameInfoEx`（22） | Win32 错误 87：INVALID_PARAMETER |

来源核对：

1. 本次 cap-primitives 3.4.6 的 Windows `rename_unchecked` 直接调用 Rust 标准库的 `std::fs::rename`。
2. [Rust 1.99.1 的 Windows 实现](https://github.com/rust-lang/rust/blob/1.99.1/library/std/src/sys/fs/windows.rs)
   在 `MoveFileExW` 返回 ACCESS_DENIED 时尝试 `FileRenameInfoEx`；若该尝试失败且不是目录非空错误，则返回原来的 ACCESS_DENIED。
3. [Wine 9.0 的 kernelbase/file.c](https://github.com/wine-mirror/wine/blob/wine-9.0/dlls/kernelbase/file.c)
   中，`SetFileInformationByHandle` 没有 `FileRenameInfoEx` 分支，落入返回 INVALID_PARAMETER 的默认分支。
4. [Rust 官方 rename 文档](https://doc.rust-lang.org/std/fs/fn.rename.html)说明：Windows 10 1607 及以上且文件系统支持 `FileRenameInfoEx` 时，目录替换可具有对应的 Unix 行为。

据此，这两个失败与 Wine 9 的 API 实现限制一致。**这不是该操作在实体 Windows 上已经通过的证明**。
原测试断言和失败记录均保留；程序没有加入先删除目标目录再改名的非原子规避操作。

## 第三方客户端与界面

libnfs 测试使用 `nfs-ls`、`nfs-cat`、`nfs-cp`，完成目录列举、fixture 内容核对以及
**164,116 字节**的上传和下载。两端 SHA256 都是：

```text
388b94bb0af0d99cb5e8548dab81dcd36e920f08ec6af19a2d7e9de1db893a4d
```

向只读共享上传时得到 `NFS3ERR_ROFS`。

GUI 验证包括空配置首次打开、添加共享并保存、启动真实后端并等待 `RUSTNFS_READY`、
复制正确的挂载命令、停止服务以及关闭窗口。截图见 [运行界面](screenshots/running.png)。
没有执行 Windows 防火墙脚本，也没有在本环境构建 MSVC 版本；交付 EXE 是 GNU Windows 目标的 release 构建。

## 复跑

准备两个一次性测试导出：`/share` 读写，`/readonly` 只读；两者都含有内容为
`RustNFS fixture` 加一个换行的 `fixture.txt`。不要把协议测试指向真实工作目录。

启动服务后运行：

```sh
python tests/protocol_smoke.py --host 127.0.0.1 --pmap-port 12011 --mount-port 12048 --nfs-port 12049 --transport both --max-record-bytes 2097152
```

Windows 文件系统额外添加 `--windows-case-rename`。这里的三个端口需与实际测试配置一致。
完整的配置、协议能力和上限见 [兼容性说明](compatibility.md)。
