# sidediff

[![CI](https://img.shields.io/github/actions/workflow/status/happy-proto/sidediff/ci.yml?event=pull_request&label=CI)](https://github.com/happy-proto/sidediff/actions/workflows/ci.yml)
[![Release](https://github.com/happy-proto/sidediff/actions/workflows/release.yml/badge.svg?branch=main)](https://github.com/happy-proto/sidediff/actions/workflows/release.yml)
[![依赖状态](https://deps.rs/repo/github/happy-proto/sidediff/status.svg)](https://deps.rs/repo/github/happy-proto/sidediff)
[![License](https://img.shields.io/github/license/happy-proto/sidediff)](LICENSE)

一个只读的 Git 差异终端界面，用于浏览当前工作区、暂存区和历史提交的左右差异。

[difftastic 内部库 fork](https://github.com/happy-proto/difftastic) 生成结构化差异，[kat](https://github.com/happy-proto/kat) 提供语法高亮与嵌套区域背景。两者均编入程序，运行时只需外部 Git。

## 安装

先安装 Git。差异引擎随 sidediff 编译，无需安装 `difft`。

从源码安装需要 stable Rust：

```sh
cargo install --path . --locked
```

使用 [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) 安装 `latest` 预编译包：

```sh
cargo binstall --git https://github.com/happy-proto/sidediff sidediff
```

预编译包支持 Linux x64（glibc）、macOS Apple Silicon、Windows x64 和 Windows ARM64。同版本重新安装时加 `--force`。

## 使用

在 Git 工作区中运行：

```sh
sidediff
sidediff --unstaged
sidediff --cached
sidediff HEAD~1 HEAD
sidediff --merge-base main feature -- src
sidediff --relative=src
sidediff -M50% HEAD -- src
```

| 输入 | 比较内容 |
| --- | --- |
| 无参数 | `HEAD → 当前工作区`，加上未跟踪文件；首次提交前以空状态为基准 |
| `--unstaged` | `index → 当前工作区` |
| `--cached` / `--staged` | `HEAD → index` |
| 一个 revision | 该 revision 与工作区 |
| 两个 revision | 两个历史快照，不加入未跟踪文件 |

`A..B`、`A...B` 和有父提交的 `commit^!` 由 Git 展开后确定历史比较端点，不会使用工作区内容替换快照。根提交的 `commit^!` 只展开一个端点，遵循 Git 的单 revision 语义，与当前工作区比较。
双栏界面中的合并提交 `commit^!` 比较第一父提交与合并结果，并在界面中注明；比较其他父提交时显式传入端点，例如 `sidediff HEAD^2 HEAD`。

`--` 后的路径相对调用目录解释。裸 `--relative` 将范围限制到调用目录；`--relative=src` 限制到仓库根目录下的 `src`，与 `--relative=src/` 等价，不包含 `src2` 等前缀相近目录；`--relative=.` 表示整个仓库。可选参数的值与选项紧贴，例如 `-M50%`、`--find-renames=50%`、`--relative=src`；空格后的输入作为 revision。

文件列表支持新增、删除、修改、重命名、复制、类型变化和冲突状态。子模块展示 commit 与工作区脏状态，不递归展开子模块内容；与普通文件互换时，同时显示文件侧的新增或删除内容。冲突视图标明未合并状态；`--cached` 不会把工作区冲突内容伪装为已暂存内容。

未跟踪的嵌套仓库展示目录元数据，不递归比较其内容。二进制冲突保留未解决状态，省略不可显示的工作区字节。

## 按键

| 按键 | 操作 |
| --- | --- |
| `q` | 退出；紧凑布局的差异页先返回文件列表 |
| `Ctrl-c` | 文件列表立即退出；差异页一秒内按两次退出 |
| `Enter` / `Esc` | 打开差异 / 返回或取消搜索 |
| `Tab`、`h` / `l` | 切换焦点 |
| `b` | 显示或隐藏侧栏 |
| `j` / `k`、方向键 | 移动或滚动 |
| `Ctrl-d` / `Ctrl-u` | 翻半页 |
| `PageDown` / `PageUp` | 翻页 |
| `g g` / `G` | 顶部 / 底部 |
| `n` / `N` | 下一个 / 上一个 hunk |
| `/` | 文件模糊搜索；搜索中普通字母均作为输入，用方向键移动 |

加载期间显示阶段、完成文件数、累计时间及正在处理的文件；可用 `q`、`Esc` 或 `Ctrl-c` 立即取消并恢复终端。鼠标捕获关闭，便于直接选择和复制终端文本。默认 tab 宽度为 4，可用 `--tab-width` 调整。差异引擎固定使用 JSON 数据和完整差异计算，`DFT_DISPLAY`、`DFT_EXIT_CODE`、`DFT_CHECK_ONLY` 不改变界面或隐藏差异；其他 `DFT_*` 配置仍可能影响计算。

默认最多 4 个文件并行计算；单侧超过 10 MiB、新旧内容累计超过 128 MiB，或超出 5,000 个计算文件时，保留文件条目并显示省略原因。上限可调整，详见 [性能与资源边界](docs/performance.md)。这些是输入限制，不是进程内存硬上限。

文件采用行级比较时，差异面板标题显示 `Diff · line diff`，文件开头用低强调文字说明原因，例如触及图搜索／字节上限、解析错误或文件类型无法进行结构化比较。标题标记在滚动和隐藏侧栏后仍然可见；语法高亮仍可保留，不代表正在使用结构化比较。

## 分析加载耗时

沿用相同的 Git 参数，只运行加载，不打开 TUI：

```sh
sidediff --profile main HEAD
sidediff --profile --json main HEAD > loading.json
sidediff --profile --cached -- src
```

报告包含阶段墙钟时间、各计算环节的累计／最大耗时、文件体积和最慢文件列表。并行环节的累计时间可能大于总耗时，嵌套环节也有重叠。终端进度写入 stderr，报告写入 stdout；Ctrl-c 会输出部分报告及尚未完成的文件，并以状态码 130 退出。详见 [加载诊断](docs/performance.md#加载诊断)。

## 动态补全

```sh
# Zsh
source <(COMPLETE=zsh sidediff)
# Bash
source <(COMPLETE=bash sidediff)
```

Fish：`COMPLETE=fish sidediff | source`。升级程序后重新加载注册钩子。

## 开发

开发任务统一由 [mise.toml](mise.toml) 管理：`mise run check` 检查格式、Clippy 与 Actions 锁文件，`mise run test` 运行常规回归，`mise run ghostty-e2e` 运行颜色回归，`mise run install` 从源码安装。

环境、端到端回归与提交检查见 [CONTRIBUTING.md](CONTRIBUTING.md)；数据流和当前边界见 [架构说明](docs/architecture.md)。本项目使用 [MIT License](LICENSE)。
