# 调研结论与开发计划

## 1. 调研

| 主题 | 来源 | 关键结论 |
|---|---|---|
| Google Reader API | FreshRSS `p/api/greader.php`、FreshAPI、Miniflux `internal/googlereader`、原 Google Reader 文档 | 路径与鉴权约定、ID 格式（16 位小写十六进制长 ID ↔ 十进制短 ID）、`ot/nt/xt/it/c/n/r` 语义、各实现的缺陷（FreshRSS 的 `xt=starred` 无效、`ot`+`nt` 用 OR 组合、包含式 continuation 丢条目等）应避免 |
| TT-RSS API | tt-rss `classes/API.php` 等、API 文档、News+ 插件 | 信封键顺序、参数类型宽松转换、特殊订阅/分类 ID、标签 ID 换算、`include_header`/`first_id`、`getCompactHeadlines`、各 API 级别差异 |
| 客户端行为 | NetNewsWire、Read You、Capy、Fluent Reader、Newsboat、TTRSS-Reader、tt-rss Android、RSS Guard、Readrops、NewsFlash、Vienna、Liferea 源码，闭源客户端的公开 issue | **没有任何 Google Reader 客户端会为已有 ID 重新下载内容** → 更新必须以新 ID 发布；部分客户端只能从列表得知已读状态 → 旧版要保留在列表中（标记已读）；不少客户端按 `ot`/`since_id` 增量同步 → ID 与入库时间必须在发布时单调分配；TT-RSS 客户端要求 32 位 ID、JSON 布尔；Readrops 要求 16 位十进制 ID |
| LLM 翻译 | PDFMathTranslate-next / BabelDOC 源码，以及 20 余个翻译项目 | 占位符保护 + 从本地 DOM 重建（模型无法破坏 HTML）、按 ID 对齐并二分重试、标签格式便于从截断输出中保留已完成段落、只缓存校验通过的结果、遵守 Retry-After、熔断；BabelDOC 的弱点（缓存未校验结果、忽略 `finish_reason`、无语言检测）需要规避 |
| 测试订阅源 | 实际抓取 | BBC 的 GUID 带版面序号 `#N` 且会变化，链接带跟踪参数；cn-bbc（kindle4rss）GUID 为 md5(标题)、正文为繁体全文且含大量 ichef 图片（webp）；本机网络使用 198.18.0.0/15 fake-IP DNS |
| Rust 生态 | crates.io 实测 | feed-rs（解析）、dom_smoothie（Readability 移植，BBC 文章 2–4 ms）、ferrous-opencc（OpenCC 官方词典纯 Rust 实现，比 zhconv 更符合 OpenCC 结果）、ammonia、dom_query、rusqlite（bundled）、axum、reqwest（rustls） |

## 2. 架构决策

1. Rust 单体 + SQLite（WAL），无外部服务；Docker 多阶段构建，rustls + 内置 SQLite，便于 amd64/arm64。
2. 持久化任务直接建模在业务表上（订阅的 `next_fetch_at`、修订的 `stage/next_attempt_at/lease_until`、故事的 `next_check_at`），不另建通用任务队列。
3. 故事（身份）/ 修订（处理）/ 文章（发布）三层分离，文章 ID 在发布时分配。
4. 内容处理模块化：提取器（内置/选择器/外部）与翻译后端（`ChatBackend` trait，OpenAI 兼容实现）可替换。
5. 设置分两层：引导配置用环境变量；其余存数据库并在后台修改、即时生效。

## 3. 开发阶段

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1 | 调研、技术选型与验证（提取、OpenCC、解析） | 完成 |
| 2 | 数据库模式、工具库、订阅解析、身份与修订判定 | 完成 |
| 3 | 内容处理：清理、全文提取、OpenCC、图片缓存、翻译引擎 | 完成 |
| 4 | 抓取调度、处理流水线、发布、原文复查、维护任务 | 完成 |
| 5 | Google Reader / TT-RSS / News+ API | 完成 |
| 6 | 集成测试：更新、客户端同步、翻译、恢复、协议一致性、图片 | 完成 |
| 7 | Web 管理后台 | 完成 |
| 8 | Docker 打包、文档 | 完成 |
| 9 | 实际验证（真实订阅、真实客户端 Newsboat、容器重启） | 完成（翻译需真实 API 密钥，见 TESTING.md） |
