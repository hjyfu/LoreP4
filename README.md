# LoreP4

LoreP4 是一个 Rust 命令行工具，用于在 Perforce Helix Core 和 Epic Games Lore 之间迁移已提交的仓库历史。它按提交重建文件版本，并从目标仓库回读校验内容。同一个文件在三次提交中有三个版本，迁移后仍可分别取出这三个版本。

[English guide](docs/english.md) · [下载发行版](https://github.com/hjyfu/LoreP4/releases) · [Epic Games Lore](https://github.com/EpicGames/lore)

## 安装

下载适合本机的发行包，或从源码安装。运行时需要能从 PATH 启动 `p4` 和 `lore`。已用 Lore 0.14.0 和 P4 2024.2 验证。连接 Unicode P4 服务时请配置 UTF-8 字符集，例如设置 `P4CHARSET=utf8`。登录信息由各自的官方 CLI 管理。

```sh
cargo install --locked --git https://github.com/hjyfu/LoreP4 --tag v0.1.0
lorep4 --help
```

## Perforce → Lore

下面的命令会读取 `//depot/game/...` 内可见的所有 submitted changelist，生成可复用的迁移包，并在新的本地目录创建 Lore 仓库。每个涉及该子树的 CL 对应一条 Lore revision。跨目录的 CL 只包含所选范围内的文件。

```sh
lorep4 p4-to-lore --port p4.example.com:1666 --user alice --depot "//depot/game/..." --repo ./game-lore --bundle ./p4-history
```

要在 Lore 服务器上创建仓库，可以在同一命令后加 `--lore-url lore://lore.example.com:41337/game`。工具完成本地回读校验后执行 push，再比对远端分支头。使用已登录且有建库权限的身份。

## Lore → Perforce

目标可以是已有 classic depot 下没有历史的子目录，或已有且文件映射直接的空 stream 根。工具创建专用 P4 client。迁移期间请独占目标路径。

```sh
lorep4 lore-to-p4 --repo ./game-lore --port p4.example.com:1666 --user alice --depot "//depot/imported-game/..." --bundle ./lore-history
```

默认读取本地 Lore 实例可见的历史。连接到 Lore 服务的实例可加 `--remote` 读取远端分支。加 `--restore-credit` 可把源作者和时间写进 P4 changelist 的原生 User 和 Date 字段，需要 P4 管理权限。未开启时，P4 显示执行迁移的用户和提交时间，原始值保存在每次提交的迁移记录中。Lore 的毫秒时间在 P4 原生字段中取整到秒。

## 哪些记录能保留

| 内容 | 迁移行为 |
| --- | --- |
| 提交与文件内容 | 保留提交边界、说明、源编号、作者、时间、增删改、空文件和二进制版本。目标系统分配自己的提交编号。每条目标提交跟踪一份 `.lorep4-history.json`，只改类型或元数据的提交也有对应历史。 |
| 移动与 P4 集成 | 同一 CL 内、范围内成对的移动重建为原生 move。保存 P4 filelog 的集成路径和版本范围。回迁时可重建范围内的单来源 branch/copy。复杂 resolve 和跨范围集成保存在记录中。 |
| Lore 分支与合并 | 遍历可见分支（含归档分支）的两个父节点，共享祖先只读一次。单分支直接落到目标根，多分支映射为 `branches/<branch-id>/`。分支名、分支头和合并父节点保存在迁移记录中。能够匹配父分支版本的文件会生成 P4 集成记录。 |
| P4 到 Lore 的映射 | 所选子树按 CL 顺序写入一条 Lore 分支。原 CL 编号和源时间也保存在 Lore revision metadata。Lore 原生时间为重放时间。P4 stream 布局和 Lore 分支图之间没有一对一的原生还原。 |
| 文件类型与目录 | 记录 P4 原文件类型。Lore 原生文件默认以 P4 binary 写入，避免文本转换；可执行模式映射为 `binary+x`。空目录及其模式可保存在 Lore 来源的迁移记录中。 |
| 边界 | 服务端账号、权限、保护表、标签、作业、搁置或待提交更改、stream 规格和工作区配置不在本版范围。源文件版本须可读。Lore 外链、P4 symlink/apple、已清理的历史、目标端会裁剪历史的文件类型等会报错。 |

迁移包和目标端的历史记录包含源作者、文件路径、提交说明，P4 集成记录还可能引用范围以外的 depot 路径。把迁移包和目标仓库的读取权限设为适合这些历史数据的范围。P4 表单可能规范化说明末尾的换行，迁移记录保留原说明。

## 分步运行与恢复

只导出和校验可以先评估一份真实仓库。下面两步对源 P4 只读，也不创建目标仓库。

```sh
lorep4 export-p4 --port p4.example.com:1666 --depot "//depot/game/..." --bundle ./p4-history
lorep4 verify --bundle ./p4-history
```

`export-lore`、`import-lore` 和 `import-p4` 可以独立运行。迁移包的 `history.json` 记录版本关系和文件操作，`blobs/` 按 BLAKE3 摘要去重存储文件内容。缺失或损坏的引用会在导入前报错。预计磁盘需要容纳去重后的历史版本和一份工作目录。省略 `--bundle` 时，直迁使用临时目录。

```sh
lorep4 import-p4 --port p4.example.com:1666 --depot "//depot/imported-game/..." --bundle ./lore-history --resume
```

`--resume` 会验证目标已有 CL 的迁移记录和文件，再从下一条继续。重复运行完整导入也不会产生重复 CL。P4 报错时会打印诊断 client 和工作目录。如果中断发生在 Lore 导入阶段，可使用保留的包在新的目标目录重放。超大仓库应先选一个有代表性的子树测量时间、磁盘和服务端请求量。

## 测试

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
python tests/roundtrip.py --cli target/debug/lorep4 --p4d /path/to/p4d
```

集成测试会在 loopback 上启动独立 P4 服务并创建临时 Lore 仓库，逐版本比较文件、作者、时间、说明和操作类型。测试包含二进制、中文和 P4 特殊字符文件名、只改类型、重命名、分支合并、续传及损坏数据。Windows 下将 `--cli` 指向 `target/debug/lorep4.exe`。
