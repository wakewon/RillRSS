# RillRSS

轻量、高效、稳定的自托管 RSS 聚合与内容处理服务。主要面向移动端阅读器使用：兼容 **Google Reader / FreshRSS API**（含 TT-RSS FreshAPI 路径）与 **TT-RSS API**（含 News+ 扩展），并在服务端完成**全文提取、繁简转换、AI 翻译、图片缓存**，以及对持续修订的新闻进行**更新检测与重新发布**。

- 单个 Rust 二进制 + SQLite，无 Redis / 消息队列 / 外部依赖；Docker 镜像支持 amd64 与 arm64。
- 只有简洁的 Web 管理后台（配置、订阅管理、任务状态、诊断），不含网页阅读器；单用户。

---

## 功能

| 功能 | 说明 |
|---|---|
| 订阅管理 | RSS 0.9x/1.0/2.0、Atom、JSON Feed；网页地址自动发现订阅；分类；OPML 导入/导出（导出时附带每个订阅的处理选项，可完整恢复） |
| 定时抓取 | 条件请求（ETag / Last-Modified）、按订阅间隔与随机抖动、同站点并发与礼貌延迟、失败指数退避、遵守 `Retry-After` |
| 阅读器 API | Google Reader（`/api/greader.php`、`/plugins.local/freshapi/api/greader.php`、根路径 `/reader/api/0`）与 TT-RSS（`/api/`、`/tt-rss/api/`，API level 25，含 News+ `getCompactHeadlines`） |
| 全文提取 | 每个订阅独立开启：内置 Readability（Rust 移植，速度毫秒级）/ CSS 选择器 / 外部提取服务；可配置移除元素；失败自动回退到订阅内容；优先使用页面 og:image 作为题图 |
| 繁简转换 | 每个订阅独立开启：OpenCC 官方词典的纯 Rust 实现，支持 t2s、hk2s、tw2s、tw2sp、s2t、s2tw、s2hk、s2twp 等；只转换正文文本与图片说明，不破坏链接 |
| AI 翻译 | 每个订阅独立开启并指定目标语言；任意 OpenAI 兼容接口；仅译文或双语对照；段落级缓存；完善的质量校验、重试与熔断（见下文） |
| 图片缓存 | 每个订阅独立开启：下载正文图片、按内容去重、返回本服务器地址；类型嗅探（不缓存 SVG）、大小与数量限制、跟踪像素过滤、私网地址防护、无引用自动清理 |
| 新闻更新 | 识别同一新闻的持续修订（订阅条目变化或原文页面变化），确认实质性修改后以**新文章 ID** 发布新版本，旧版标记已读，收藏/标签/备注随新版转移；冷却期合并、版本上限、跟踪时长限制 |
| 稳定性 | 持久化处理队列（崩溃后自动恢复）、租约 + 心跳、各阶段独立并发、翻译限流与熔断、单个订阅或翻译服务故障互不影响 |
| 管理后台 | 概览、订阅与处理选项、分类、文章与版本记录、任务队列与失败重试、事件日志、全部设置、诊断工具（测试提取 / 翻译 / 订阅地址）、账户与 API 专用密码 |

---

## 快速开始（Docker Compose）

```bash
git clone <this repo> rillrss && cd rillrss
cp .env.example .env          # 修改 RILL_PASSWORD 等
docker compose up -d --build
```

然后访问 `http://服务器:8080/admin`，用 `.env` 中的用户名与密码登录（若未设置密码，首次启动会在日志中打印随机密码：`docker compose logs rillrss`）。

数据（SQLite 数据库与图片缓存）保存在 `./data`。容器以 `PUID`/`PGID`（默认 1000）运行，启动时会自动修正数据目录权限。

### 常用环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RILL_USERNAME` | `admin` | 管理与 API 用户名 |
| `RILL_PASSWORD` | 空 | 首次启动时的初始密码（之后在后台修改） |
| `RILL_BASE_URL` | 空 | 手机访问到的公开地址（如 `https://rss.example.com`），用于缓存图片的地址；留空则按请求的 Host / X-Forwarded-* 自动推断 |
| `RILL_TRUST_PROXY` | `true` | 信任反向代理的 X-Forwarded-* 头 |
| `RILL_ALLOW_PRIVATE_NETWORKS` | `false` | 允许从局域网地址下载原文网页与图片（订阅本身始终允许局域网地址） |
| `RILL_TRANSLATE_BASE_URL` / `_API_KEY` / `_MODEL` | 空 | 默认翻译服务（也可在后台设置） |
| `RILL_LOG` | `info` | 日志级别，如 `info,rillrss=debug` |
| `RILL_DATA_DIR` / `RILL_LISTEN` | `/data` / `0.0.0.0:8080` | 数据目录与监听地址 |

其余所有设置（抓取、全文、翻译、图片、更新检测、保留策略）都在后台「设置」页面修改，立即生效并保存在数据库中。

### 反向代理（HTTPS）

阅读器建议通过 HTTPS 访问。Caddy 示例：

```
rss.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Nginx 需要传递转发头（并且**不要**对 API 路径做重定向——重定向会让 POST 请求丢失内容）：

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    client_max_body_size 32m;
}
```

---

## 连接阅读器

在后台「概览」页可以看到适用于当前访问地址的连接信息。设 `https://rss.example.com` 为服务地址：

| 客户端 | 账户类型 | 服务器地址 |
|---|---|---|
| Reeder、NetNewsWire、Read You、FocusReader、FeedMe、Fluent Reader、Capy Reader、Readrops、NewsFlash、Vienna | FreshRSS / Google Reader | `https://rss.example.com/api/greader.php` |
| 以 FreshAPI（TT-RSS 插件）方式配置的客户端 | FreshRSS / Google Reader | `https://rss.example.com/plugins.local/freshapi/api/greader.php` |
| Inoreader 风格（只填主机）的客户端 | Google Reader | `https://rss.example.com` |
| TTRSS-Reader、tt-rss Android、News+、Fiery Feeds、RSS Guard、Newsboat | Tiny Tiny RSS | `https://rss.example.com`（客户端自动追加 `/api/`；也可用 `https://rss.example.com/tt-rss`） |

用户名为 `RILL_USERNAME`；密码为管理密码，或在「账户」页设置的 **API 专用密码**（推荐为手机单独设置）。

---

## 内容处理

每个订阅的处理顺序：**全文提取 → 清理 / 题图 / 繁简转换 → AI 翻译 → 图片缓存 → 发布**。一篇文章只有在全部处理完成后才对阅读器可见，并在发布时才分配文章 ID，因此阅读器永远不会先缓存到未翻译的版本，基于 `since_id` / `ot` 的增量同步也不会漏掉晚完成的文章。

### AI 翻译

- 将 HTML 切分为块级段落，行内标签（链接、加粗、图片、代码等）替换为编号占位符 `<g1>…</g1>`、`<x2/>`，翻译后再按原始 DOM 还原，**模型不可能破坏 HTML 结构**，图片与链接不会丢失。
- 默认以 `<seg id="N">` 标签批量请求（输出被截断时仍可保留已完成的段落），也可切换为 JSON 格式。
- 逐段质量校验：缺失 / 空白、占位符丢失或错位、未翻译（含"半翻译"整句残留）、原文与译文混杂、重复循环、过短（截断）/过长、相邻段错位。
- 失败的段落拆小批次重试，直到单段；单段最多 3 次尝试；仍失败的保留原文并在后续重试中只补译缺失段落（段落级缓存）。
- 传输层：并发上限、每分钟请求数、指数退避并遵守 `Retry-After`、区分额度耗尽 / 鉴权错误；连续失败触发熔断，暂停期间文章排队而不占用资源。
- 已是目标语言的文章自动跳过；推理模型的 `<think>` 内容会被剔除；支持附加提示词与附加请求参数（如 `{"enable_thinking": false}`）。
- 失败策略可选：超过尝试次数后发布原文（附说明），或继续等待（最长 72 小时）。

### 新闻更新

1. **识别同一新闻**：按规范化 GUID 匹配；GUID 变化但链接相同且旧 GUID 已不在订阅中时视为同一新闻（适配 GUID = md5(标题) 的订阅）；去除跟踪参数；BBC 这类 `url#N`（版面序号）GUID 自动合并；同一文档中多条目共用链接时不做合并。
2. **判断实质修改**：去除"N 小时前"等易变内容后，用词/汉字 3-gram 计算相似度与新增量；错别字修正、标题微调、版面变化不会触发新版本（阈值可调）。启用全文时，条目变化会重新提取原文确认；「页面追踪」模式还会在首次出现后按时间表（默认 30 分钟、1.5、4、10、24、48 小时）复查原文页面。
3. **发布新版本**：新版本使用新的文章 ID、标题前缀 `[更新]`、正文顶部附更新说明（含首发时间），发布时间使用更新时间以便出现在列表顶部；旧版标记为已读但**仍保留在列表中**（Read You、Newsboat 等客户端只能通过列表得知已读状态），收藏保持，并把收藏/标签/备注带到新版本。
4. **防止重复与覆盖**：冷却期内的多次修改合并为一次发布；版本数上限；只追踪首次出现后一定时长内的更新；正在处理的旧修订一旦被新修订取代，就永远不会发布（旧翻译不会覆盖新版）。

---

## 运维

- **健康检查**：`GET /healthz`；镜像内置 `HEALTHCHECK`（`rillrss healthcheck`）。
- **备份**：运行中可执行 `docker compose exec rillrss entrypoint.sh backup /data/backup-$(date +%F).db`（SQLite 在线备份，得到一致的快照）；或停止容器后复制 `./data` 目录。OPML 导出也包含所有订阅与处理选项。
- **完整性检查**：`docker compose exec rillrss entrypoint.sh check-db`（输出 `ok` 表示正常）。
- **不要在容器运行时用宿主机上的工具直接打开数据库**：SQLite 的 WAL 模式依赖共享内存，宿主机与容器（尤其是 Docker Desktop 的虚拟机文件共享、NAS、网络文件系统）同时访问可能损坏数据库。请使用上面的备份命令。
- **升级**：`git pull && docker compose up -d --build`；数据库自动迁移。
- **重置密码**：`docker compose exec rillrss entrypoint.sh reset-password 新密码`（通过 `entrypoint.sh` 执行可保证以服务用户身份运行）。
- **清理**：每小时自动执行（文章保留天数、每订阅上限、旧版本、过期翻译缓存、无引用图片、日志），也可在「任务」页手动执行。收藏永不删除。
- **日志**：`docker compose logs -f rillrss`；后台「日志」页记录抓取、更新、翻译、发布等事件。

### 多架构镜像

```bash
docker buildx build --platform linux/amd64,linux/arm64 -t your-registry/rillrss:latest --push .
```

---

## 开发

```bash
cargo build --release
RILL_DATA_DIR=./data RILL_PASSWORD=dev-password ./target/release/rillrss
cargo test            # 单元测试 + 集成测试（内置模拟订阅服务器与模拟 OpenAI 接口）
```

文档：

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 架构、数据模型、处理流水线、协议兼容性决策
- [docs/PLAN.md](docs/PLAN.md) — 调研结论与开发计划
- [docs/TESTING.md](docs/TESTING.md) — 自动化测试与实际验证记录

## 已知限制

见 [docs/TESTING.md](docs/TESTING.md#已知限制)。

## 许可证

MIT。协议实现参考了 FreshRSS、FreshAPI、Tiny Tiny RSS、TT-RSS News+ 插件与多个开源客户端的公开行为，未复制其代码（这些项目为 AGPL/GPL 许可）。
