# 仓库约定

## 维护语言

- 本仓库的文档、代码注释、PR、Issue 和 review 交流默认使用中文；代码标识符、命令、协议名称及必要的第三方原文保持原样。
- Git commit message 继续使用英文和 Conventional Commits 格式。
- 若 squash 合并默认继承中文 PR 标题，合并时将最终提交标题改为英文 Conventional Commits；PR 标题仍使用中文。
- 用户当次明确指定的语言优先；对外协作若有目标平台的硬性格式要求，遵循该要求。
- GitHub 仓库级合并设置遵循 `dcjanus:dcjanus-merge-policy`。该策略不代替分支保护或 CI 配置。
- 实现决策优先考虑减少故障、方便扩展和降低长期维护成本；默认不为历史实现保留兼容层，即使需要较大的短期重构。

## 参考项目

按需参考 [difftastic](https://github.com/Wilfred/difftastic.git)、[Git](https://github.com/git/git.git)、[Zed](https://github.com/zed-industries/zed.git)、[Codex](https://github.com/openai/codex.git) 和 [kat](https://github.com/happy-proto/kat.git)。必要时把参考仓库克隆到 `.external/`，不提交该目录。

## 测试

- 常规测试遵循仓库 [mise.toml](mise.toml)，使用 `mise run test`；不要自行拼凑 `nextest` 参数，也不要无故改用 `cargo test` 跑全量测试。
- 默认命令：`cargo nextest run --config-file .config/nextest.toml --cargo-quiet --status-level fail --final-status-level fail --failure-output final --success-output never --show-progress none --no-tests pass`。
- 单个测试的超时与宽限期由 [`.config/nextest.toml`](.config/nextest.toml) 统一配置；当前为 1 分钟、宽限期 `0s`。
- 提交检查使用 `mise run precommit`，依次运行格式检查、Clippy 和常规测试。
- Ghostty 终端端到端测试使用 `mise run ghostty-e2e`，依赖本地 `difft`、Git 和该任务声明的 Zig 工具链。
