# Writing Insights 性能测量

日期：2026-10-12 · 平台：Windows 11 x64（GNU 工具链，dev profile）· 版本：0.6.0-dev（personal 分支）

## 方法

- 构造 2000 段测试文档（1200 段英文 + 800 段中文，共 65,200 词 / 21,600 个 CJK 字符 / 282,400 字符），经桌面端控制端口逐段插入后 `file.saveAs` 保存为 docx（9.4 KB 压缩）。
- 各命令跑 3 次取观察值，`date +%s%N` 计时（含进程启动、docx 解析、分析与输出）。
- 基线 `text`（只做打开 + 纯文本导出）与目标 `insights`（打开 + 全量分析 + 报告渲染）之差近似分析开销。

## 结果

| 命令 | run 1 | run 2 | run 3 |
|---|---|---|---|
| `insights big.docx` | 273 ms | 268 ms | 255 ms |
| `text big.docx`（基线） | 125 ms | 128 ms | 133 ms |

分析净开销 ≈ 130 ms / 28 万字符（≈ 0.46 µs/字符）。

## 结论

- 交互场景（桌面面板在文档变更时重算）对该规模文档约 0.1 s，典型文档（2-5 万字符）在 10-25 ms 量级，无感知卡顿；报告按 `session.rev` 缓存后滚动、选择零开销。
- 当前实现为单线程全量重算；若未来需要处理更大的文档（>1M 字符），可按段落并行（`rayon` 已在工作区依赖中）或增量分析变更段落，现阶段没有必要。

## 复现

```sh
wordcraft-cli insights TEMP/big.docx   # 3 次
wordcraft-cli text TEMP/big.docx       # 基线 3 次
```
