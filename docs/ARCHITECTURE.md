# RillRSS 架构说明

## 1. 总体结构

单个 Rust 进程（tokio 异步运行时）+ 一个 SQLite 数据库（WAL 模式）+ 图片缓存目录：

```
                ┌──────────────── axum HTTP 服务 ────────────────┐
  阅读器 ──────▶│ Google Reader API │ TT-RSS API │ /img /feed-icons│
  浏览器 ──────▶│ Web 管理后台（askama 模板，Cookie 会话 + CSRF）   │
                └───────────────┬────────────────────────────────┘
                                │ store（查询/状态修改）
  ┌──────────────┐   ┌──────────▼──────────┐   ┌───────────────────┐
  │ 抓取调度器    │──▶│ SQLite（WAL）        │◀──│ 处理调度器         │
  │ feeds.next_   │   │ feeds stories        │   │ extract→transform→ │
  │ fetch_at 驱动 │   │ story_keys revisions │   │ translate→images→  │
  └──────────────┘   │ items labels images  │   │ publish（各阶段独立 │
  ┌──────────────┐   │ translations events  │   │ 并发、租约+心跳）   │
  │ 原文复查器    │──▶│ sessions settings    │   └───────────────────┘
  └──────────────┘   └─────────────────────┘   ┌───────────────────┐
  ┌──────────────┐                              │ 翻译服务（限流、熔断、│
  │ 维护任务      │（保留策略、GC、裁剪日志）      │ 段落缓存、质量校验） │
  └──────────────┘                              └───────────────────┘
```

模块（`src/`）：

| 模块 | 职责 |
|---|---|
| `feeds/parse.rs` | feed-rs 解析 RSS/Atom/JSON Feed；补充解析 feed-rs 忽略的条目级 `media:thumbnail` |
| `feeds/ingest.rs` | 新闻身份识别与修订判定（见 §3） |
| `feeds/fetcher.rs` | 调度与抓取、条件请求、退避、订阅发现、增删订阅 |
| `pipeline.rs` | 修订处理流水线与发布、原文复查、重处理 |
| `content/` | `sanitize`（ammonia 白名单 + 懒加载图片/picture/noscript/iframe 处理）、`extract`（dom_smoothie / 选择器 / 外部服务）、`convert`（OpenCC）、`translate/`、`images` |
| `api/greader.rs`, `api/ttrss.rs` | 两套阅读器协议 |
| `store.rs` | 文章查询构建器、状态修改、订阅/分类/标签列表 |
| `web.rs` + `templates/` | 管理后台 |
| `maintenance.rs` | 定期清理 |
| `http.rs` | HTTP 客户端（SSRF 防护 DNS 解析器、重定向检查、体积限制、编码识别）、同站点限速 |

## 2. 数据模型

- **feed**：订阅，含每订阅处理选项（JSON：全文、OpenCC、翻译、图片、更新追踪、保留天数、UA）与抓取状态。
- **story**：订阅中的一条新闻（跨所有版本）。`story_keys` 记录它出现过的所有身份键：`g:<规范化 GUID>`、`u:<规范化链接>`。
- **revision**：一次待处理的源快照，同时就是**持久化任务队列**：`state`（pending/published/unchanged/failed/obsolete）、`stage`（extract/transform/translate/images/publish）、`attempts`、`next_attempt_at`、`lease_until`，以及每个阶段完成后的中间结果（正文 HTML、标题、源文本）。
- **item**：已发布、对阅读器可见的文章。`items.id`（AUTOINCREMENT，永不复用）就是对外文章 ID，**在发布时分配**；`created_at`（微秒）为入库时间，严格单调递增。

为什么在发布时分配 ID：文章需等待全文/翻译/图片完成后才可见。若在抓取时分配 ID，晚完成的文章会拿到比已同步文章更小的 ID，基于 `since_id`（TT-RSS）或 `ot`（Google Reader）的客户端就会永久漏掉它。

## 3. 新闻身份与更新判定

**身份**（`ingest.rs`）：

1. GUID 规范化：URL 型 GUID 末尾的短数字片段（BBC 的 `#1`、`#2` 是首页版面序号，会随版面变化）被去除；其他片段（如直播帖 `#post-12`）保留。
2. 链接规范化：去掉协议、`www.`、默认端口、片段、跟踪参数（`utm_*`、`at_*`、`fbclid`、`ocid` 等），参数排序，去尾斜杠。
3. 匹配顺序：GUID 键 → 链接键（仅当该故事的所有 GUID 都已不在当前文档中，即 GUID 发生了"漂移"，例如 cn-bbc 订阅的 GUID 是 md5(标题)，改标题即换 GUID）。同一文档中多个条目共用同一链接时，该链接不用于匹配。
4. 首次抓取只处理最新 `initial_items` 条，过旧条目记为 skipped（之后不会被当作新文章）。

**实质修改判定**（`util/text.rs`）：文本规范化（NFKC、小写、去除"5 hours ago / 3 小时前 / 阅读时间"等易变片段）后，以 3-gram（西文为词、中日韩为字）计算 Jaccard 相似度和新增/删除量。满足以下任一条件即为实质修改：变化比例 ≥ 10% 且变化量 ≥ min(25, 原文长度×0.3)；或新增 ≥ 80 个 3-gram（约一段）；或标题明显改变（相似度 < 0.7）且正文也有变化。仅标题改变默认不算（可配置）。

**流程**：

- 未启用全文：抓取时直接用订阅内容判定，不满足则忽略。
- 启用全文：条目变化只作为线索，修订先进入 extract 阶段重新提取原文，提取后与上一版源文本比较，未实质改变则标记为 `unchanged`。
- 「页面追踪」模式：按时间表复查原文页面（默认首次出现后 30 分钟、1.5/4/10/24/48 小时），检测到实质变化时直接生成 `page_update` 修订（已带提取结果）。
- 冷却：距上次发布不足 `min_interval_min` 的修订推迟执行；期间再有变化，旧的待处理修订被标记为 `obsolete`，由最新源替代（合并）。
- 版本上限与跟踪时长：超过 `max_versions` 或首次出现超过 `track_hours` 后不再发布新版本。

## 4. 处理流水线（`pipeline.rs`）

调度器每个周期为每个阶段按空闲并发数**认领**到期修订（`UPDATE … SET lease_until = now+租约 … RETURNING id`），处理期间每分钟续租。每个阶段结束时用 `WHERE id=? AND state='pending' AND stage=?` 比较并更新（CAS），因此：

- 进程崩溃后，租约过期即可被重新认领；启动时直接清空所有租约。每个阶段的结果都已落库，重启后从中断的阶段继续；翻译有段落缓存，已译段落不会重复请求。
- 修订在处理中被新修订取代（obsolete）时，其后续阶段的 CAS 必然失败，发布事务还会再次确认"没有更新的待处理修订"，所以**旧翻译永远不会覆盖新版本**。
- 认领时先把 `unfinished` 加一，阶段有结果后清零：某篇文章若让进程崩溃（例如栈溢出），重启后最多再试 3 次就标为失败，不会反复拖垮服务。阶段内的意外错误或 panic 退避重试，5 次后标为失败；心跳用 drop guard，阶段异常退出也会停止续租，续租只对同一阶段生效。

| 阶段 | 内容 | 失败处理 |
|---|---|---|
| extract | 抓取原文（同站点并发与间隔限制）→ 嵌套深度预检 → Readability / 选择器 / 外部服务 → 移除元素 → 长度校验；更新修订在此确认实质修改；页面 og:image 作为题图；相对地址按最终网址与页面 `<base>` 解析 | 新文章：临时错误退避重试 3 次，之后回退到订阅内容。条目更新：原文是确认更新的唯一依据，约 4 小时内退避重试 7 次。重新处理：当前版本来自原文时绝不用订阅内容替换，保留当前版本 |
| transform | 移除元素 → 清理（白名单、绝对链接、懒加载图片、picture/noscript、视频 iframe 转链接、删空元素）→ 无图时插入题图 → OpenCC | — |
| translate | 见 §5 | 按尝试次数退避（1、5、15、30 分钟…），熔断期间不计次数；超过 `max_attempts` 后按策略发布原文或继续等待 |
| images | 下载、嗅探类型、去重存储、改写地址；超过总容量时按文章从旧到新淘汰缓存图片（收藏文章最后），这些文章改回原图地址，`/img/` 对已淘汰的图片 302 到原图 | 单图失败保留原地址；整体错误重试 3 次 |
| publish | 单事务：分配 ID、版本号、标题前缀与更新说明、状态转移、旧版处理、图片引用、故事状态 | — |

**发布新版本时**（同一事务内）：新版本 `is_read = 0`（可配置为继承）、继承收藏/发布标记/备注/标签、发布时间取更新时间；旧版本 `is_read = 1`、`read_at = now`、`superseded_by = 新 ID`，**保留收藏、默认不隐藏**。

**重新处理**（修改设置后按新设置重新生成）发布的版本不是新闻：继承已读状态（含 `read_at`）、收藏与原发布时间，不加前缀，已读的旧版直接隐藏；它不计入 `stories.news_versions`（版本上限、冻结与更新说明里的 vN 都用它），`stories.version` 仍是保证 guid 唯一的内部计数。已有待处理修订（例如等待冷却的新闻更新）的新闻不再排入重新处理，以免新闻更新被当作重新处理悄悄发布；若当前版本已被清理则取消；翻译失败或不完整时标为失败并保留当前版本。

`stories.from_page` 记录当前版本及其比较基准来自原文网页还是订阅条目，更新判定总是同类比较。旧文本至少 50 个词、新文本不到一半时不算实质更新：这类大幅缩水通常是付费墙、同意页或错误页，而非新闻修订。

## 5. 翻译引擎（`content/translate/`）

1. **切分**（`segment.rs`）：在 DOM 上找"只含行内内容"的块（p、li、h*、figcaption、td…）以及混合内容中的行内片段；行内元素变成占位符：成对标签 `<gN>…</gN>`（链接、加粗等，保留其文本供翻译）、原子占位符 `<xN/>`（图片、换行、行内代码，原样保留）。`pre`、`code`、`script` 等不翻译；纯数字/URL 段落跳过。
2. **缓存**：键 = blake3(命名空间, 目标语言, 提示词版本, 段落源文本)，命名空间 = 模型 + 附加提示词 + 附加请求参数的哈希（两者为空时与旧键相同）；只缓存通过校验的结果，批次中途出错时已通过校验的段落照样缓存。
3. **批量请求**：按字符数与段数分批（默认 4000 字符 / 16 段），系统提示词固定（利于提供商的提示缓存），默认 `<seg id="N">` 格式；可选 JSON 格式（`response_format`）。
4. **解析**：去除 `<think>`/代码块，按 `<seg>` 或多种 JSON 结构解析，容忍前后多余文字；截断输出中已闭合的段落仍可使用。
5. **校验**（`validate.rs`）：缺失/空、占位符（缺失、重复、嵌套错误、凭空出现）、未翻译（目标为中日韩时检查字符比例与残留的整句原文；西文之间用语言检测）、夹带原文、重复循环（4-gram 重复率）、长度比例异常（截断/冗余）、相邻段输出相同（错位）。`finish_reason=length` 时最后一段不可信。
6. **降级阶梯**：整批失败 → 二分拆批；部分失败 → 只重发失败段；单段最多 3 次（换用"上次被拒"的严格提示）；仍因占位符失败则接受译文并由还原逻辑丢弃损坏的标签、补回图片等原子占位符；短的未翻译段（专名）接受原文；其余记为失败，保留原文，等待后续重试。
7. **传输**：并发信号量 + 每分钟令牌桶；429/5xx/超时指数退避并遵守 `Retry-After`/`retry-after-ms`（非有限值丢弃，最长 1 小时）。
8. **错误分类与熔断**：
   - 参数、模型、Key、额度错误（含智谱 1113/1304 等）第一次出现即暂停 600 秒起；
   - "输入过长"与内容审核（含 DeepSeek "Content Exists Risk"、OpenRouter 403 审核）拆批定位到单段，保留原文；
   - 不认识的 400 先当作与内容有关并拆批，与此前失败段落毫不相干的请求再次出现同样错误时才视为配置问题；
   - 很短的请求也超长、空输出且 `finish_reason=length`、反复无法解析的输出，都视为配置问题（提示 max_tokens、附加提示词或推理模型）；
   - 其他提供商故障连续 5 次打开 30 秒起，倍增至 30 分钟。
   打开期间翻译阶段不认领任务；到期后半开，只放行一个试探请求。熔断器带代数：熔断状态变化或保存设置之前发出的请求，其结果一律忽略。每次打开最多升级一次。概览页显示导致暂停的那条错误与提示。

## 6. 协议兼容性要点

经过对 FreshRSS、FreshAPI、Miniflux、TT-RSS、News+ 插件源码，以及 NetNewsWire、Read You、Capy Reader、Fluent Reader、Newsboat、TTRSS-Reader、tt-rss Android、RSS Guard、Readrops、NewsFlash、Vienna、Liferea 等客户端源码的调研：

- **两套 ID 空间**：TT-RSS 使用内部 ID（多个客户端按 32 位整数解析）；Google Reader 短 ID = 10^15 + 内部 ID（16 位十进制，Readrops 依赖），长 ID 为 `tag:google.com,2005:reader/item/` + 16 位小写十六进制。输入兼容长格式、无前缀十六进制（Reeder）、十进制（含有符号）。
- **时间**：`crawlTimeMsec`/`timestampUsec`（字符串）= 入库时间；`ot`/`nt` 按入库时间过滤且不加余量；`published`/TT-RSS `updated` 为数字。读状态查询（`it=read` + `ot`）同时返回 `ot` 之后被标记已读的文章，使 Read You 等能得知旧版本已读。
- **分页**：键集分页，`continuation` 为字符串且无更多数据时省略；`items/ids` 每页最多 10000，`stream/contents` 最多 1000；`items/contents` 不返回 continuation。
- **鉴权**：ClientLogin 纯文本 `SID/LSID/Auth`；令牌由服务器密钥与密码哈希派生（无 `=`、长期有效、改密码即失效）；`T` 参数宽松（缺失、`x`、带换行均接受；无 Authorization 头时也可用 `T=令牌` 认证）；401 同时带 `Google-Bad-Token` 与 `X-Reader-Google-Bad-Token`；写操作返回无换行的 `OK`。
- **JSON 细节**：项目字段不出现 null；`itemRefs` 只含 `id` 且总是存在；订阅必有分类（未分类归入 "Uncategorized"）；TT-RSS 信封键顺序 `seq,status,content`，布尔值为 JSON 布尔，未打标签为 `tags: [""]`，标签为 `[id, caption, fg, bg]`，标签订阅 ID = -1025 - label_id；`include_header`/`first_id_changed`、`getCompactHeadlines`、`getFeedTree`、`getCounters` 等格式与 TT-RSS 一致。
- **路径**：`/api/greader.php/…`、`/[tt-rss/]plugins.local/freshapi/api/greader.php/…`、根路径 `/accounts/ClientLogin` 与 `/reader/api/0/…`；TT-RSS `/api`、`/api/`、`/api/index.php` 及 `/tt-rss/` 前缀；图标 `/feed-icons/<id>.ico`、`/public.php?op=feed_icon&id=<id>`、`getFeedIcon`。

## 7. 安全

- 密码 Argon2id，同时最多 2 个密码校验（每个约 19 MiB）。网页会话 Cookie（HttpOnly、SameSite=Lax、HTTPS 时 Secure）与 CSRF 令牌（表单字段，不放在网址里）。
- 登录失败限速：每 IP 15 分钟内 10 次，尝试在校验前就计数，并发猜测也受限。客户端 IP 只在来自私网的连接（反向代理）上读取 `X-Forwarded-For`，并从右往左取第一个公网地址，伪造的左侧条目无效。
- 可设置独立的 API 密码；修改密码使 Google Reader 令牌与 TT-RSS 会话失效。令牌与会话绑定到校验时匹配的密码哈希，与改密码同时进行的登录也拿不到有效令牌。
- 请求体上限：默认 2 MiB，Google Reader OPML 导入与后台 OPML 上传 16 MiB，TT-RSS 512 KiB，ClientLogin 等公开路径 64 KiB；Google Reader 在读取请求体前先检查 Authorization 头；每个请求最多解析 20,000 个参数。
- 管理后台 CSP 只允许带本次响应 nonce 的内联脚本；外部链接只渲染 http(s) 地址；订阅地址、公开地址、翻译与提取服务地址只接受 http(s)。
- 原文网页、图片、图标请求禁止访问私网/回环/链路本地等地址：每一跳重定向都会解析并检查，配置了代理时也先在本机解析检查；198.18.0.0/15 因代理软件 fake-IP 模式而放行。可通过 `RILL_ALLOW_PRIVATE_NETWORKS` 关闭。订阅只有在填写的地址本身是本机或局域网时才可访问私网，公网订阅的重定向与自动发现都不能进入私网。
- HTML 嵌套深度有上限：原文提取预检 512 层、清理后 200 层、翻译切分 100 层，深层嵌套的页面不会让进程栈溢出。
- 图片只缓存经魔数识别的位图格式，响应带 `nosniff` 与 `sandbox` CSP；正文 HTML 经 ammonia 白名单清理，链接只保留 http(s)。
- 容器以非 root 用户运行。
