# 开发与贡献

文档、代码注释及 PR/Issue 默认使用中文；标识符与命令保持原样。提交标题使用英文 Conventional Commits。项目规范见 [AGENTS.md](AGENTS.md)。

## 环境

- 通过 rustup 安装 stable Rust；[rust-toolchain.toml](rust-toolchain.toml) 声明 rustfmt 与 Clippy。
- 安装 Git、difftastic 0.71.0 和 [mise](https://mise.jdx.dev/)。
- 在仓库运行 `mise install`，安装 [mise.toml](mise.toml) 声明的 cargo-nextest；Ghostty 任务按需安装声明的 Zig 工具链。
- 开发任务统一使用 `mise run <任务>`，无需启用 shell 集成。

```sh
mise install
mise run precommit
```

`mise run precommit` 依次检查格式、Clippy 和常规测试，不会自动修改源码。也可单独运行 `mise run fmtcheck`、`mise run lint`。构建与安装使用锁文件：

```sh
cargo build --release --locked
mise run install
```

## 测试原则

新增测试优先覆盖已经发生并复现的错误，不以覆盖率或测试数量为目标。修复前先确认测试因目标错误失败，再保留能验证修复的回归；不要为私有字段、内部调用顺序或可替换实现补断言。现有必要测试继续保留。

常规入口：

```sh
mise run test
```

[terminal_regressions.rs](tests/terminal_regressions.rs) 启动实际二进制，在独立的临时 Git 仓库和 PTY 中发送按键，用终端模型检查屏幕与退出行为。夹具隔离用户 Git 配置，不修改开发者仓库。它覆盖已复现的路径、首次提交、状态过滤、冲突、子模块、搜索、退出和大文件滚动错误。

颜色叠加另有 [Ghostty 端到端测试](tests/ghostty_terminal_e2e.rs)：

```sh
mise run ghostty-e2e
```

该入口使用 [mise.toml](mise.toml) 声明的 Zig 与 Cargo 锁定的 Ghostty bindings，首次运行会下载并编译 Ghostty。它通过 PTY 启动实际 sidediff，把 ANSI 输出送入 Ghostty 终端内核，检查语法前景色与差异、嵌套区域背景的叠加结果；无需打开 Ghostty 应用窗口。Linux CI 也执行该入口。工具链错误应记录系统、SDK 与 Zig 版本及失败日志，不修改系统 SDK 或清理项目 target。

日常功能回归用 vt100 终端模型检查屏幕文本、按键和退出行为；Ghostty 用于需要最终单元格颜色证据的回归。架构边界及各层验证方式见 [架构文档](docs/architecture.md#分层验证)。两类测试共同覆盖用户可见行为；新增回归仍以实际发生的错误为依据，不要求每层机械地增加测试数量。

所有测试共用 [nextest 配置](.config/nextest.toml) 的一分钟单测超时；失败时继续收集本批结果。不要另拼全量测试命令或在测试中无限等待子进程。

## 工作流与发布

CI 对默认分支、PR 和 merge queue 的合并组执行检查，并为发布目标生成归档。PR 通过 merge queue 合入；`latest` 仅由默认分支验证成功的提交更新。四个平台的支持情况以各平台真实 CI 结果为准，本机测试通过不表示其他平台已经验证。

依赖维护由 [Dependabot](.github/dependabot.yml) 每周提出更新。零主版本依赖的 minor 变更单独审查，不与普通兼容更新混在一起。
