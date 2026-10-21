# RustNFS 0.1.2：兼容性与协议范围

更新日期：2026-10-21。

RustNFS 是面向开发局域网、设备文件传输和普通目录共享的 Rust NFSv3 实现。
这一版提供 Windows x64 应用与可在 Linux 上运行的同一协议核心。它使用独立的
程序、配置和开源协议代码，不依赖其他 NFS 产品的激活服务。本说明不对其他厂商的
经营或维护状态作判断。

下面的“实现”描述以本版本源代码为依据；具体构建、协议回归、Windows/Wine
执行和界面验证结果另见 [测试结果](test-results.md)。某个 RPC 测试通过，不等同于
所有 Linux 内核、BusyBox 固件或本机文件系统都已经完成兼容性验证。

## 版本与来源

| 项目 | 本次版本或来源 |
| --- | --- |
| 应用 | `rustnfs 0.1.2` |
| Rust edition | 2024 |
| manifest 中声明的 Rust 最低版本 | 1.88；本次不据此声称已用该最低版本验证 |
| 实际构建编译器 | `rustc 1.99.1 (58cf8f9c3 2026-10-13)` |
| Windows 构建目标 | `x86_64-pc-windows-gnu` |
| Linux 构建/测试目标 | `x86_64-unknown-linux-gnu` |
| 协议上游 | [Vaiz/nfs3](https://github.com/Vaiz/nfs3) |
| 导入提交 | `f4c6abdcf9304299648681fe5a9fd9cfd6bcbee7` |
| vendor 包版本 | `nfs3_server 0.11.0`、`nfs3_types 0.5.2`、`nfs3_macros 0.5.0` |
| 上游导入记录 | [vendor/UPSTREAM.txt](../vendor/UPSTREAM.txt) |
| 精确依赖与许可证 | [许可证清单](../licenses/README.md)、根 `Cargo.lock` |

应用自有代码使用 MIT；`nfs3_server` 保留 BSD-3-Clause，另外两个 vendor 包保留
Unlicense。其他依赖按原许可证分发，详见 [THIRD-PARTY-NOTICES.txt](../THIRD-PARTY-NOTICES.txt)。
源码和二进制分发材料都应保留相应许可证与 `licenses/` 原文目录。

## 0.1.2 连接设置与升级

GUI 的“连接设置”提供 Portmapper、MOUNT、NFS 三个端口输入项，均接受 1–65535，
且三项必须各不相同。“保存设置”可单独保存而不启动服务；“恢复默认端口”只把
三项分别改回 111、20048、2049，保留共享、客户端等其他设置，仍需保存设置或启动服务。

端口占用（Windows 错误 10048）时，可停止占用该端口的原服务，或选用空闲端口。
启动日志会给出失败服务、TCP / UDP、实际监听地址及端口，并保留底层系统错误；
若某个端口绑定失败，会释放此前已经绑定的其他端口，不报告服务就绪。
11111 / 20049 / 2050 是按 Portmapper / MOUNT / NFS 顺序给出的候选值，不保证未被占用。
界面生成的挂载命令使用当前端口；修改后应重新复制命令，并更新已有防火墙规则。

升级前停止服务并关闭 GUI，仅替换两个 EXE，保留已有 `rustnfs.json`；配置格式不变。
普通账户需具备共享目录所需的读写权限；创建 Windows 防火墙规则仍须手动使用管理员权限。
本次及历史版本的实际验证结果见 [测试结果](test-results.md)。

### 保留的 0.1.1 启动修复

0.1.0 用 `ReOpenFile` 建立目录属性句柄，真实 Windows 会拒绝目录并返回
`ERROR_ACCESS_DENIED`；Wine 9 曾允许该调用，使旧测试漏掉启动失败。对照依据见
[Wine 10.12 的 `test_ReOpenFile`](https://github.com/wine-mirror/wine/blob/wine-10.12/dlls/kernel32/tests/file.c)。
自 0.1.1 起改为获取安全目录句柄的最终路径、以 `CreateFileW` 打开属性句柄并核对对象身份；
实际目录操作的 cap-std 保护保持不变，不修改 ACL，也不要求提权。

## 已实现的协议

服务仅监听 IPv4。应用启动时，同时绑定下列每个服务的 TCP 和 UDP 端口；其中任何一个
端口失败，启动就失败，避免只启动一部分服务。端口可在 GUI 或配置文件中修改。

| 服务 | RPC 程序 / 版本 | 默认端口 | 实现范围 |
| --- | --- | --- | --- |
| Portmapper | 100000 / 2 | 111，TCP + UDP | NULL、GETPORT、DUMP；按程序、版本和 TCP/UDP 返回准确映射；SET/UNSET 返回 false，CALLIT 不提供 |
| MOUNT | 100005 / 3 | 20048，TCP + UDP | NULL、MNT、DUMP、UMNT、UMNTALL、EXPORT |
| NFS | 100003 / 3 | 2049，TCP + UDP | 下表列出的 NFSv3 文件操作 |

| 操作 | 本版行为 |
| --- | --- |
| NULL、GETATTR、LOOKUP | 提供；无效、已删除或被不同对象替换的句柄返回错误 |
| READ、WRITE | 普通文件读写；每次 I/O 上限 8192 字节，WRITE 校验 count 与数据长度 |
| CREATE UNCHECKED | 创建或打开普通文件，应用支持的初始属性 |
| CREATE GUARDED | 使用原子 `create_new(true)`；已存在返回 `NFS3ERR_EXIST` |
| CREATE EXCLUSIVE | 返回 `NFS3ERR_NOTSUPP`；未伪造持久 verifier |
| MKDIR、REMOVE、RMDIR、RENAME | 提供；REMOVE 与 RMDIR 分开检查对象类型；跨导出 RENAME 返回 XDEV |
| SETATTR | 支持本节下文所述的大小、时间和 mode 投影；uid/gid 只能与导出固定映射一致 |
| ACCESS | 根据属性 mode 与只读状态返回受支持权限位；不是逐用户的主机账户鉴权 |
| READDIR、READDIRPLUS | 使用有界内存快照和 cookie/verifier；无效续读位置返回 BAD_COOKIE，过小的回复空间返回 TOOSMALL |
| FSINFO、PATHCONF | 报告本版 I/O、文件名等限制；Windows 报告大小写不敏感 |
| FSSTAT | 通过主机 API 获取空间统计；Windows 文件/inode 总数无法由此 API 得到，返回 0 |
| COMMIT | 对对应普通文件调用同步；WRITE 本身也执行 `sync_all`，成功时回复 FILE_SYNC |

NFSv3、MOUNTv3 的协议格式及语义参考 [RFC 1813](https://www.rfc-editor.org/rfc/rfc1813.html)；
RPC 消息及 TCP record marking 参考 [RFC 5531](https://www.rfc-editor.org/rfc/rfc5531.html)。
本版只实现表中范围，不宣称完整实现所有 NFSv3/POSIX 行为。

### 导出路径

配置中的共享别名例如 `share`、`data` 对应客户端路径 `/share`、`/data`。
MOUNT 从虚拟根查找完整路径，不把任意请求路径都映射到同一个真实目录。
可以挂载导出目录内部的子目录；相似前缀 `/share-extra` 不属于 `/share`。
客户端路径中的 `..`、反斜杠、盘符等形式不能用于 MOUNT 越界。

LOOKUP 的 `..` 在一个导出的根部停留在该根，不能通过它切换到另一个导出。
共享别名并不改变主机操作系统对底层真实目录的权限要求。

## 必须了解的语义限制

### EXCLUSIVE 与文件锁

NFSv3 的 EXCLUSIVE CREATE 要求持久保存 verifier。无法提供这种语义时，
[RFC 1813 §3.3.8](https://www.rfc-editor.org/rfc/rfc1813.html#section-3.3.8)
允许返回 `NFS3ERR_NOTSUPP`。RustNFS 采用这一返回值。

2026-10-20 查阅的 Linux 主线 [`nfs3_proc_create`](https://github.com/torvalds/linux/blob/master/fs/nfs/nfs3proc.c)
在收到该错误后具有 EXCLUSIVE → GUARDED 的回退逻辑；这不能证明未知设备的旧内核或
裁剪固件也具备同样行为。需要 EXCLUSIVE 创建语义的应用必须以实际客户端测试为准。

本版没有 NLM/NSM 锁服务，也没有实现跨客户端的分布式文件锁。NLM 在
[RFC 1813 附录 II](https://www.rfc-editor.org/rfc/rfc1813.html#section-6) 中是独立协议。
使用 `nolock` 只能让客户端按无 NLM 的方式挂载，不能补出锁功能。
不适用于依赖共享数据库锁、多个主机并发更新同一数据库或类似语义的用途。

### 权限与身份

每个导出使用配置中的固定 UID/GID。服务器不会根据客户端提交的 AUTH_SYS UID/GID
切换 Windows/Linux 主机用户，也不提供按客户端用户分别设置 ACL 的功能。
AUTH_SYS 字段不是身份凭证；接入控制依赖 IPv4 白名单，文件 I/O 最终受运行程序的主机账户权限约束。
本版没有 Kerberos、RPCSEC_GSS 或传输加密，应部署在适合这种访问模型的受控网络中。

`file_mode`、`dir_mode` 及 SETATTR 的 mode 是返回给客户端的权限投影。SETATTR mode
仅存于当前进程的节点记录中，重启后恢复导出默认值；不会变成持久的 Windows ACL，
也不会在服务器端提供完整的逐用户 Unix mode 强制检查。
`read_only=true` 则是后端真实执行的写入限制，不依赖客户端是否服从 ACCESS 结果。

Windows 上的 ctime 由修改时间近似表示。大小、时间、mode 的组合修改没有数据库式
回滚；遇到真实的磁盘/权限/同步错误时，客户端应重新读取属性确认结果。

### 对象与文件名

支持普通文件和目录。SYMLINK、READLINK 不支持，LINK、MKNOD 不提供；设备节点、FIFO、
socket、符号链接和 Windows reparse point 不作为普通共享对象使用。
本版不提供 NFSv2、NFSv4、IPv6、ACL/xattr 协议或网络根文件系统启动功能。

不提供 LINK RPC 不代表会拆开主机已有的硬链接：已经存在的普通文件硬链接仍引用同一个
底层对象。需要严格隔离共享内容时，使用没有指向共享外文件的硬链接的专用目录。

文件名必须是有效 UTF-8，同时不超过 255 个 UTF-8 字节和 255 个 UTF-16 code unit。
为保持 Windows 路径行为一致，还拒绝 Windows 保留设备名、冒号、斜杠/反斜杠、控制字符、
尾随空格/点及其他禁止字符。因此，并非所有合法的 Linux 本地文件名都能通过本版共享。
不能表示或不受支持的目录项可能从目录枚举中省略。

Windows 路径缓存按大小写不敏感的模型工作；本版没有为启用了每目录大小写敏感特性的
Windows 目录提供独立兼容保证。文件 identity 使用 cap-fs-ext 提供的 device + 64 位
file index；该依赖对 ReFS 的 128 位 file ID 存在明确说明，故不将 ReFS/SMB 映射盘的
句柄行为视作已验证能力。相关依据可在本次依赖的 `metadata_ext.rs` 与
[cap-std 3.4.6 上游](https://github.com/bytecodealliance/cap-std) 查看。

### Windows 目录覆盖重命名与 Wine 9

将非空源目录重命名为一个已经存在的空目标目录，依赖主机提供对应的原子重命名语义。
[Rust 1.99.1 的 `std::fs::rename` 文档](https://doc.rust-lang.org/1.99.1/std/fs/fn.rename.html)
明确限定：Windows 10 1607 及以上、并且文件系统支持 `FileRenameInfoEx` 时，才有与 Unix
一致的目录覆盖行为。本版面向 Windows 10/11 x64；不会先删除目标目录再重命名来模拟这一操作。

Wine 9 的 [`SetFileInformationByHandle` 实现](https://github.com/wine-mirror/wine/blob/wine-9.0/dlls/kernelbase/file.c)
没有 `FileRenameInfoEx` 分支。独立于服务器的 Windows probe 中，直接调用该 class 22
返回 Win32 错误 87，而同布局的 `FileRenameInfo` class 3 正向对照成功；纯
`std::fs::rename` 覆盖空目录返回原始 Win32 错误 5。普通目录重命名成功，是否持有本版
形式的目录/子目录/文件 pin 不改变这组结果。

因此 Wine 9 中 TCP、UDP 各一项目录覆盖测试的失败保留为环境限制，不改写为通过。
对应主机能力、原始结果与其他测试结论见 [测试结果](test-results.md)。实际 Windows
目标文件系统仍应验证这一项，不能把 Wine 的失败或 Rust 文档的条件性说明直接推广成
所有主机都支持或都不支持。

### 句柄、重启和重试

节点记录与 NFS 句柄映射存于内存。删除或同名替换后，旧句柄应失效；服务器重启后，
客户端可能需要重新挂载。由客户端经服务器执行的重命名会更新已知节点路径。
本机其他程序直接重命名共享对象时，不提供自动跟踪所有旧 NFS 句柄的功能。

Windows 的长期目录 identity pin 使用 metadata-only 句柄：先对安全打开的目录调用
`GetFinalPathNameByHandleW`，再以 `CreateFileW`、`FILE_READ_ATTRIBUTES`、
`FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT` 和共享 READ/WRITE/DELETE
建立新句柄，核对其 device/file index 与原目录一致后才保存。该句柄只保留对象身份。
目录遍历和目录操作仍使用临时 cap-std `Dir`，在其有效期内保留 cap-std 默认的
不共享 DELETE 保护；不会把允许重命名的 metadata pin 当成目录能力使用。

UDP 去重缓存按完整请求、客户端 IP/端口和服务匹配，能够返回已完成请求的原始响应，
并阻止相同请求并发执行。缓存是有界、限时、非持久的，缓存淘汰或重启后不保证
“恰好执行一次”。TCP 也不提供跨断线重连的持久事务日志。

写入和 COMMIT 的同步行为由当前主机文件系统及其系统调用实现。这里不声称已经完成
每个磁盘、文件系统和断电场景的持久性认证。停止服务会关闭新连接、打断空闲读取并等待
已经开始的文件操作完成，不在正常停止时取消写到一半的操作。

## 本版资源上限

以下列出应用当前实际值；不是所有底层 transport 可配置字段都暴露在应用配置里。
定义分别位于 [`src/config.rs`](../src/config.rs)、[`src/bin/rustnfsd.rs`](../src/bin/rustnfsd.rs)、
[`src/filesystem.rs`](../src/filesystem.rs) 和 [`transport.rs`](../vendor/nfs3_server/src/transport.rs)。

| 项目 | 当前值 |
| --- | --- |
| 导出数量 | 1–32 |
| 白名单条目 | 1–64 个 IPv4/IPv4 CIDR |
| TCP 连接数 | 默认 32，应用配置允许 1–128，三项服务合计 |
| 同时执行的 RPC 请求 | 16，三项服务及两种传输合计 |
| 整条 TCP record 读取/空闲超时 | 30 秒；回复写入也设超时 |
| TCP record 大小 | 最多 2 MiB |
| 每条 record 碎片数 | 最多 256 |
| UDP 消息大小 | 最多 60 KiB |
| READ / WRITE 数据 | 每次最多 8192 字节 |
| 写入末尾及 SETATTR size | 不超过 1 TiB，即 2^40 字节 |
| UDP 完成请求缓存 | 最多 256 项 / 16 MiB，TTL 120 秒 |
| MOUNT 记录 | 最多 128 项 / 48 KiB |
| 已注册对象句柄 | 最多 32,768 个 |
| 单次目录快照 | 最多 20,000 个可处理名称 |
| 保留的目录快照 | 最多 8 个，TTL 120 秒 |
| 单个 XDR opaque 解码分配 | 最多 2 MiB；另有传输整体限制 |
| XDR `Vec<u32>` 解码 | 最多 65,536 个元素 |

对象句柄达到上限、目录过大或快照已过期时，服务会返回错误而不是无限扩张内存。
客户端应能处理 JUKEBOX、BAD_COOKIE 等返回值；目录续读失效后从 cookie 0 重新开始。
已注册节点持有打开的对象 pin，实际可达到的数量还受主机句柄/文件描述符上限影响。

## 客户端起始参数

以下是支持 NFSv3 的 Linux 客户端的起始配置示例。服务器地址、共享别名和本地挂载点
需要换成实际值；客户端还必须包含对应的内核支持和挂载工具。

TCP：

```sh
mount -t nfs -o vers=3,proto=tcp,mountproto=tcp,mountvers=3,nolock,rsize=8192,wsize=8192 192.168.1.10:/share /mnt/share
```

UDP：

```sh
mount -t nfs -o vers=3,proto=udp,mountproto=udp,mountvers=3,nolock,rsize=8192,wsize=8192 192.168.1.10:/share /mnt/share
```

如果设备只实现 NFSv2，它不能通过更换传输参数变成 NFSv3 客户端。
BusyBox 的具体选项支持取决于固件构建；必要时检查设备上的 `mount --help` 和内核能力。
自定义端口需要同步设置客户端的 `port=` / `mountport=`，并使网络策略允许所选传输。

首次接入设备时，用一个专门测试目录验证其实际需要的读、写、创建、重命名、删除和
重连行为。权限投影、锁或特殊文件是需求的一部分时，应先对照以上范围判断是否满足。

## 本地协议补丁

以下描述本次相对于导入上游的改动，不代表这些行为均来自原上游版本。

| 区域 | 主要变化 |
| --- | --- |
| `transport` | 新增可停止的 IPv4 TCP/UDP 三服务监听、统一白名单、连接/请求/超时/消息限制 |
| RPC / XDR | TCP 碎片与累计长度检查；opaque/整数数组分配前限长；认证字段和程序版本检查 |
| UDP 重放 | 完整请求与 peer/service 键、有界 LRU/TTL、正在执行请求去重 |
| Portmapper / MOUNT | 返回实际 TCP/UDP 映射；多导出、组件边界验证、目录类型检查及六项 MOUNTv3 过程 |
| VFS 接口 | 增加真实 `fsstat`、独立 `rmdir`、原子 `create_guarded`、可预检属性的 `mkdir_with_attrs` |
| NFS handlers | READ/WRITE/FSINFO 大小限制；REMOVE/RMDIR 分派；ACCESS/PATHCONF 错误传播与属性修正；目录续读校验 |
| 应用文件后端 | cap-std 目录能力、对象 identity pin、Windows metadata-only 目录 pin、重命名节点更新、属性预校验和按句柄操作 |

这一版的能力由当前源码和测试结果共同界定。后续改变依赖版本、传输边界或文件系统语义时，
应同步更新本文、[测试结果](test-results.md) 和 [许可证清单](../licenses/README.md)。
