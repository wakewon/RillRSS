# 测试与验证

## 1. 自动化测试

```bash
cargo test
```

共 120 个测试，全部在本地完成，无需网络：集成测试在进程内启动真实的 RillRSS（临时数据目录、随机端口），以及一个模拟上游（订阅、原文网页、图片）和一个模拟 OpenAI 兼容接口（可按脚本返回 429/5xx、乱码、截断、未翻译、慢响应）。

| 文件 | 数量 | 覆盖内容 |
|---|---|---|
| 单元测试（`src/**`） | 89 | URL/GUID 规范化、文本相似度与实质修改判定、HTML 清理（懒加载、picture、noscript、iframe、srcset）、Readability/选择器提取、OpenCC、图片嗅探与路径校验、翻译切分/占位符还原/双语、输出解析（`<seg>`、JSON、`<think>`、截断）、质量校验（未翻译、半翻译、重复、过短、夹带原文、错位）、翻译降级阶梯、限流器、熔断、身份识别（BBC `#N`、GUID 漂移、共用链接）、首次抓取限制、冷却、设置表单、TT-RSS 参数转换与信封顺序、GReader ID/流/路径解析、数据库迁移与备份 |
| `tests/updates.rs` | 5 | 实质更新以新 ID 发布（旧版已读、仍在列表、收藏与标签转移、标题前缀、更新说明）；错字/版面/标题微调/GUID 漂移不产生重复；冷却期合并与版本上限；原文页面更新检测；条目变化经原文确认 |
| `tests/sync.rs` | 4 | 模拟真实客户端同步算法：NetNewsWire/Capy 式（未读 ID 差集 + `ot` + 只为缺失 ID 拉取内容）、TTRSS-Reader/News+ 式（`since_id` 分页 + `getCompactHeadlines` 未读同步）、Fluent Reader 式（`stream/contents` + `ot`）、Read You 式（一个月内已读 ID 查询需包含被取代的旧版） |
| `tests/translation.rs` | 8 | 端到端翻译与结构保持、双语模式、传输/内容故障重试至完整、持续失败按策略发布原文、hold 策略与熔断、更新时只翻译变化段落、**慢翻译被新修订取代时旧翻译不会发布**、已是目标语言跳过、OpenCC 与标题 |
| `tests/recovery.rs` | 3 | 模拟崩溃（租约未释放、阶段中断）后重启自动恢复且不重复；ID 永不复用；单个订阅故障退避不影响其他订阅 |
| `tests/api_greader.rs` | 6 | 登录与 401 头、多根路径、令牌格式、`T` 宽松处理；订阅/标签/未读数；分页与 continuation、各种 ID 格式、`xt/it` 组合、标签、`kept-unread`、`mark-all-as-read` 的 `ts`；订阅编辑/重命名/删除标签、quickadd、OPML 导入导出；缓存图片地址随请求主机生成 |
| `tests/api_ttrss.rs` | 4 | 信封与错误码、各路径与 Content-Type 宽松、base64 密码兼容、注销；分类/订阅/计数器/订阅树；`getHeadlines` 字段顺序与类型、`include_header`、`since_id`、排序与 `skip`、视图模式、`getCompactHeadlines`、`updateArticle`、`getArticle`、标签、`catchupFeed`；订阅与退订 |
| `tests/images.rs` | 1 | 去重（不同 URL 相同内容只存一份）、跟踪像素移除、超大图/SVG/伪装图片不缓存、已下载 URL 不重复请求、无引用后 GC 删除文件 |

连续运行完整测试 8 次无失败（检测并发相关的不稳定测试）。

## 2. 实际验证（2026-10-20）

### 2.1 测试订阅源

| 订阅 | 配置 | 结果 |
|---|---|---|
| BBC News `https://feeds.bbci.co.uk/news/rss.xml` | 全文提取 + 页面追踪 | 31 条条目中有同一文章以 `#1/#2` 出现于不同版面的情况，合并后 28 篇；全部成功提取全文（平均正文 4.4 KB，约 2–4 ms/篇，受同站点 500 ms 礼貌间隔限制，共约 18 秒）；题图使用页面 og:image（高清）而非 RSS 的 240px 缩略图 |
| BBC 中文 `http://feedmaker.kindle4rss.com/feeds/cn-bbc.xml` | t2s 繁转简 + 图片缓存 + 移除署名区块 | 5 篇全部转为简体（标题、正文、图片说明 alt）；31 张 ichef 图片（WebP，共 766 KB）缓存到本地，阅读器得到 `https://<本服务>/img/…` 地址；GUID 为 md5(标题)，改标题不会重复 |

注意：本机网络使用代理软件的 fake-IP DNS（所有域名解析到 198.18.0.0/15），最初图片下载被私网防护拦截；已调整为放行该保留网段（它不承载内网服务），并补充了说明。

### 2.2 真实新闻更新检测

BBC 订阅以「页面追踪」模式运行约 1 小时，期间 BBC 网站实际修订了两篇文章，均被正确识别并以新 ID 发布 v2（旧版自动标为已读）：

| 文章 | 判定依据 | 实际变化 |
|---|---|---|
| Chancellor rejects calls to raise Capital Gains Tax in Budget | 相似度 0.64，+122 / −25 | 新增 4 段（税率说明、引语），改写 2 段 |
| England unchanged to face France after Tonga defeat | 相似度 0.36，+527 / −9 | 初版是以 "More to follow." 结尾的快讯（14 段），更新为 40 段完整报道 |

同期 BBC 首页版面调整导致大量条目的 GUID 序号（`#1`→`#2` 等）变化，没有产生任何重复文章。

### 2.3 API 返回内容

- Google Reader：`stream/contents`、`items/ids`、`items/contents` 字段与类型符合各客户端要求（长 ID 16 位小写十六进制、短 ID 16 位十进制、时间字段为字符串、无 null）；缓存图片地址随 `Host`/`X-Forwarded-*` 生成。
- TT-RSS：`getHeadlines`（含 `include_header`）、`getArticle`、`getCounters`、`getFeedTree` 等输出类型正确（布尔为 JSON 布尔、`tags:[""]`）。

### 2.4 真实客户端：Newsboat 2.45

使用 Newsboat 的 FreshRSS（Google Reader）后端与 TT-RSS 后端分别连接：

1. 两种后端均同步到全部 33 篇文章，未读数一致。
2. 添加一个本地可控的订阅，Newsboat 同步到该新闻；随后修改该新闻（新增三段、改标题）并刷新。
3. 再次同步：两种后端都显示旧版 **已读**、新版 `[更新] …` **未读**，未读总数不变（−1 +1），与设计一致。

### 2.5 Docker

- `docker compose up -d --build`（arm64 原生构建）：健康检查通过；服务以 uid/gid 1000 运行。
- amd64 镜像通过交叉编译构建（Apple Silicon 上 QEMU 中的 x86_64 rustc 会崩溃，故 Dockerfile 在构建平台上原生编译并交叉链接），在模拟环境中运行通过健康检查与登录。
- 处理过程中 `docker compose restart`：重启前已发布 15 篇，重启后继续处理至全部完成；随后 `--force-recreate` 重建容器；`check-db` 为 `ok`，在线备份成功；最终 32 篇文章对应 32 个故事，无重复，无错误事件。
- 发现并记录：在 Docker Desktop 中宿主机工具与容器同时访问 WAL 数据库会损坏数据库（共享内存不跨虚拟机）。已禁用 mmap、增加容器内的 `backup` / `check-db` 命令，并在 README 中警示。

### 2.6 规模与性能（release 构建，Apple M 系列）

10 个订阅 × 1000 篇（共 10,000 篇，每篇约 6 段正文）：

| 项目 | 结果 |
|---|---|
| 抓取 + 处理 + 发布 10,000 篇 | 37.7 秒 |
| 内存（RSS） | 处理后约 100 MB，大量 API 请求后约 170 MB（已随后调低 SQLite 缓存与连接数） |
| `unread-count` | 17 ms |
| `items/ids` 未读 10,000 条 | 7 ms |
| `stream/contents` 1000 篇含正文 | 98 ms |
| `items/contents` 250 篇 | 4 ms |
| TT-RSS `getHeadlines` 200 篇含正文 / `skip=9000` | 12 ms / 11 ms |
| `getCompactHeadlines` 10,000 条 | 4 ms |
| `getCounters` / `getFeedTree` | 26 ms / 24 ms |
| `mark-all-as-read` 10,000 篇 | 292 ms |

据此精简了存储：已发布修订不再重复保存源文本；超过更新跟踪期的故事丢弃比较基线（并相应停止追踪）。

### 2.7 翻译

翻译引擎与流水线通过模拟 OpenAI 兼容接口完成了全部故障场景测试（见 1）。**本环境没有可用的真实 API 密钥，也未安装本地模型，因此尚未对真实大模型进行端到端验证。** 配置真实接口后，可在后台「设置 → 测试翻译服务」或「工具 → 测试翻译」立即验证，翻译统计（请求、失败、缓存命中、Token）显示在概览页。

## 3. 已知限制

- 真实大模型翻译未在本环境验证（见 2.7）；不同提供商的输出习惯可能需要调整批大小、温度、附加提示词或附加参数（如关闭思考模式）。
- 未在真实手机阅读器上实测；兼容性依据对开源客户端源码的分析、模拟同步测试与 Newsboat 实测。闭源客户端（Reeder、FeedMe、FocusReader、Fiery Feeds 等）的个别行为无法完全确认。
- 内置全文提取不执行 JavaScript、不带登录 Cookie；需要渲染或付费墙的网站请配置外部提取服务或使用订阅自带内容。
- 并发处理的文章按完成顺序发布，ID 顺序与文章发布时间不完全一致（客户端通常按时间排序，不影响增量同步）。
- TT-RSS 的评分、文章标签（tags）、`shareToPublished`、"已归档"订阅未实现；分类只有一层。搜索仅按标题与摘要做子串匹配。
- 不对图片转码（WebP/AVIF 原样提供，较老的 iOS/Android 版本可能无法显示 AVIF）。
- 未限制每日翻译 Token 预算，仅有并发与速率限制。
- 服务需部署在（子）域名根路径下，不支持路径前缀部署。
