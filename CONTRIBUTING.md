# 开发与贡献

文档、代码注释及 PR/Issue 默认使用中文；标识符与命令保持原样。提交标题使用英文 Conventional Commits。项目规范见 [AGENTS.md](AGENTS.md)。

## 环境

- 通过 rustup 安装 stable Rust；[rust-toolchain.toml](rust-toolchain.toml) 声明 rustfmt 与 Clippy。
- 安装 Git、difftastic 0.71.0 和 [mise](https://mise.jdx.dev/)。
- 在仓库运行 `mise install` 安装开发工具。
- 开发任务统一使用 `mise run <任务>`，无需启用 shell 集成。

```sh
mise install
mise run check
mise run test
```

提交前分别运行 `mise run check` 和 `mise run test`：前者检查格式、Clippy 与 Actions 锁文件，后者执行常规测试。构建与安装使用锁文件：

```sh
cargo build --release --locked
mise run install
```

## 测试原则

新增测试优先覆盖已经发生并复现的错误，不以覆盖率或测试数量为目标。修复前先确认测试因目标错误失败，再保留能验证修复的回归；不要为私有字段、内部调用顺序或可替换实现补断言。现有必要测试继续保留。

涉及路径、Git 状态或终端交互的修改，提交前运行常规测试：

```sh
mise run test
```

涉及语法颜色、差异背景或嵌套区域的修改，再运行颜色回归：

```sh
mise run ghostty-e2e
```

遇到工具链错误时，记录系统、SDK、Zig 版本及失败日志，不修改系统 SDK 或清理项目 target。沿用仓库的测试命令与超时配置，不另拼全量测试命令，也不让测试无限等待子进程。

## 工作流与发布

PR 和 merge queue 的合并组运行 Actions lock 校验、格式检查、Clippy 与常规测试（含 PTY 回归）。默认分支额外执行 Ghostty 颜色回归、四个平台的 release 构建、发布二进制回归及打包，全部通过后更新 `latest`。PR 通过 merge queue 合入。

### Actions lock

修改 workflow 或第三方 action 引用后，更新并校验 [actions.lock](.github/workflows/actions.lock)，与配置变更一起提交：

```sh
mise run actions-lock-update
mise run check
```

依赖维护由 [Dependabot](.github/dependabot.yml) 每周提出更新。零主版本依赖的 minor 变更单独审查，不与普通兼容更新混在一起。
