# 架构与边界

## 数据流

1. [cli.rs](../src/cli.rs) 解析 Git 比较参数，保留调用目录与展示路径前缀。
2. [git_diff.rs](../src/git_diff.rs) 调用 Git CLI 获取 NUL 分隔的 raw records。默认聚合模式合并工作区与未跟踪状态；暂存与历史比较读取相应对象。
3. 在内容读取前按大小、累计体积和文件数准入；gix 读取历史 blob 及 Git attributes，工作区读取原文件或符号链接目标文本。字节直接交给 difftastic 库，普通文件不物化差异内容临时文件。默认最多 4 个文件并行计算，按 Git 原始顺序收集结果。
4. difft JSON 决定行对齐、差异范围与 hunk；[kat_adapter.rs](../src/kat_adapter.rs) 通过 kat 的 `PreparedDocument` 接口提供高亮与嵌套区域背景。高亮适配不承担差异计算。
5. [model.rs](../src/model.rs) 合成左右行、折行、搜索与导航状态；[ui.rs](../src/ui.rs) 用 Ratatui 绘制当前可见行。变更底色优先于嵌套区域背景。
6. [app.rs](../src/app.rs) 在加载线程工作时处理取消输入；[subprocess.rs](../src/subprocess.rs) 使用 Unix 进程组或 Windows Job Object 回收被取消的 Git 及其子进程。终端退出守卫恢复备用屏幕、光标与 raw mode；库内正在执行的单文件计算需要完成后才能回收加载线程。

Git 路径在访问、比较与去重时保留原始字节；不可解码字节、控制字符和 U+FFFD 等需要消歧的字符只在展示时转义。

文本读取与当前 difft 的解码顺序保持一致：有效 UTF-8、带 BOM 的有效 UTF-16、少量无效字节的 UTF-8，再尝试 Windows-1252；二进制分类仍由 difft 完成。位置计算、换行检测与高亮都使用解码后的同一份文本。

[completion.rs](../src/completion.rs) 使用 gix 查询 revision 和路径，注册机制使用 clap_complete 的动态 API。它在普通参数解析和终端输出之前运行。

kat 使用 [kat-parsers](https://github.com/happy-proto/kat-parsers) 统一提供预生成 parser 和按语言首次加载的表解压。普通 Cargo 构建自动使用压缩产物；sidediff 仅通过锁文件更新 kat 及其传递依赖。上游 grammar、scanner 和许可证的批量同步与重新生成由 kat-parsers 集中维护。

库自身的锁文件不会传播到应用。直接与间接 Git revision 由 [同一可复用工作流](https://github.com/happy-proto/kat-parsers/blob/bec51e20519976597dae0e7702c93a267a3eed37/docs/git-dependencies.md) 定时批量更新，本仓库只保留调用入口。更新分支会显式运行 CI，并在执行摘要提供创建 PR 的链接。

## 分层验证

| 边界 | 职责 | 验证方式 |
| --- | --- | --- |
| Git 与差异输入 | 状态、revision、原始路径、对象字节与 attributes；调用差异库 | 在独立 Git 仓库中启动真实二进制，检查最终显示的状态与内容 |
| 文档与显示模型 | kat 高亮适配、左右行对齐、折行、搜索和导航位置 | 保留必要的模型与适配器测试；用户可见错误优先通过终端回归验证 |
| 终端界面 | 输入、焦点、滚动、颜色合成、取消与退出 | PTY 发送真实按键，vt100 检查屏幕文本；Ghostty 终端内核检查最终单元格颜色 |
| 构建与安装 | 发布二进制、归档与安装路径 | CI 通过 `SIDEDIFF_E2E_BIN` 对实际 release 二进制运行同一套功能回归，并校验归档内容 |

功能端到端回归见 [terminal_regressions.rs](../tests/terminal_regressions.rs)，颜色回归见 [ghostty_terminal_e2e.rs](../tests/ghostty_terminal_e2e.rs)。两者都通过伪终端运行真实程序，不需要打开终端应用窗口；执行入口与新增测试原则见 [CONTRIBUTING.md](../CONTRIBUTING.md)。

## 特殊状态

- 新增与删除的缺失侧使用空字节和空设备路径元数据，不读取同名无关工作区文件。
- 暂存删除或重命名后重建的 HEAD 路径在临时 index 中恢复，由 Git 原生算法计算重写、重命名和复制；真实 index 与用户文件不参与写入。
- mode `160000` 是子模块 gitlink，只显示指针与脏状态，不作为父仓库 blob 读取。
- 子模块与普通文件互换时，gitlink 侧保留元数据，文件侧显示新增或删除内容。
- 合并提交的 `commit^!` 在双栏界面中比较第一父提交与合并结果；其他父提交需要显式指定。
- 未跟踪的嵌套仓库目录只显示元数据，不传给文件差异引擎。
- 文件被目录替换时保留删除记录，目录内新增文件独立显示；FIFO 等特殊文件不作为文本读取。
- 符号链接冲突显示链接目标字符串；工作区读取不递归进入符号链接目录。
- 未合并记录保留 `U` 状态；未暂存视图可显示工作区冲突内容，暂存视图明确说明 index 尚未解决。
- 二进制显示元数据；mode、末尾换行与 CRLF/LF 格式变化也有可读说明。
- Git 报告变动而 difft 没有语法差异时，显示明确说明，避免空白差异页。
- 文件系统与命令执行错误会报告失败，不静默丢弃整条记录。

## 当前边界

- 结构化差异依赖 [happy-proto/difftastic](https://github.com/happy-proto/difftastic) 的内部 JSON／字节接口，上游基线为 0.71.0；具体 Git 提交由 Cargo.lock 锁定。库接口、上游同步和 parser 兼容性由 fork 维护。
- 文件超过 256 KiB 时跳过语法高亮，仍计算和显示差异。该阈值不是整个文件加载的内存上限。
- 默认最多 4 个文件并行，完成后进入完整浏览界面；准入限制和可调整参数见 [性能与资源边界](performance.md)。差异结果仍全部保留，输入预算不能直接换算为进程 RSS 上限。
- 渲染缓存保留当前文件的完整布局，但每帧只复制可见行，滚动位置不受 u16 行数上限限制。
- 子模块不递归 diff，冲突不提供三方合并编辑；程序是只读查看器。
- 鼠标事件关闭；侧栏为平面文件列表，不提供目录折叠。

这些边界以实际源码为准。普通内容通路不创建临时文件，但聚合枚举仍可能创建临时 Git index，因此不能声称整个进程零磁盘写入。
