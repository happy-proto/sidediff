# sidediff

一个只读的 Git 差异终端界面，用于浏览当前工作区、暂存区和历史提交的左右差异。

[difftastic](https://github.com/Wilfred/difftastic) 生成结构化差异，[kat](https://github.com/happy-proto/kat) 提供语法高亮与嵌套区域背景。`kat` 编入程序；运行时仍需外部 `git` 和 `difft`。

## 安装

先安装 Git 和支持 JSON 输出的 difftastic。当前验证使用 **difftastic 0.71.0**；程序启动时会探测 JSON 能力。上游预编译包见 [difftastic Releases](https://github.com/Wilfred/difftastic/releases/tag/0.71.0)。

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

加载期间可用 `q`、`Esc` 或 `Ctrl-c` 取消。鼠标捕获关闭，便于直接选择和复制终端文本。默认 tab 宽度为 4，可用 `--tab-width` 调整。程序明确使用 difft 的 JSON 输出，`DFT_DISPLAY=inline` 不会切换界面布局；内部调用固定 `DFT_EXIT_CODE=false`，其他 `DFT_*` 设置仍可能影响差异计算。

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
