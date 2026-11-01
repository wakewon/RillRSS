# RillRSS

轻量、高效、稳定的自托管 RSS 聚合与内容处理服务。主要面向移动端阅读器使用：兼容 **Google Reader / FreshRSS API**（含 TT-RSS FreshAPI 路径）与 **TT-RSS API**（含 News+ 扩展），并在服务端完成**全文提取、繁简转换、翻译（AI / Google / 微软）、图片缓存**，以及对持续修订的新闻进行**更新检测与重新发布**。

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
| 翻译 | 每个订阅独立开启并指定目标语言；任意 OpenAI 兼容接口，或免费、无需 Key 的 Google / 微软翻译，可设回退引擎；仅译文或双语对照；段落级缓存；完善的质量校验、重试与熔断（见下文） |
| 图片缓存 | 每个订阅独立开启：下载正文图片、按内容去重、返回本服务器地址；类型嗅探（不缓存 SVG）、大小与数量限制、跟踪像素过滤、私网地址防护、无引用自动清理；超过总容量时先删除最旧文章的图片（这些文章改回原图地址），新图片照常缓存 |
| 新闻更新 | 识别同一新闻的持续修订（订阅条目变化或原文页面变化），确认实质性修改后以**新文章 ID** 发布新版本，旧版标记已读，收藏/标签/备注随新版转移；冷却期合并、版本上限、跟踪时长限制 |
| 稳定性 | 持久化处理队列（崩溃后自动恢复）、租约 + 心跳、各阶段独立并发、翻译限流与熔断、单个订阅或翻译服务故障互不影响 |
| 重新处理 | 修改订阅或全局设置后，可把已发布的文章按新设置重新处理：单个订阅或全部订阅，范围为最近 10 / 50 / 200 / 1000 篇、最近 1 / 7 / 30 天或全部文章，可只处理未读；新版本保留已读、收藏与原发布时间 |
| 管理后台 | 概览、订阅与处理选项、分类、文章与版本记录、任务队列与失败重试、批量重新处理、事件日志、全部设置、诊断工具（测试提取 / 翻译 / 订阅地址）、账户与 API 专用密码；浅色 / 深色主题，适配手机 |

---

## 部署

### 方式一：拉取预构建 Docker 镜像（推荐）

安装 Docker 和 Docker Compose v2 或更新版本后，只需下载两个配置文件即可部署。镜像由 GitHub Actions 编译，支持 amd64 / arm64，Docker 会自动选择服务器对应的架构。

**1. 准备配置文件**

```bash
mkdir -p rillrss && cd rillrss
curl -fL https://raw.githubusercontent.com/wakewon/RillRSS/main/docker-compose.yml -o compose.yml
curl -fL https://raw.githubusercontent.com/wakewon/RillRSS/main/.env.example -o .env
```

**2. 修改 `.env`**

用文本编辑器打开 `.env`，将 `RILL_PASSWORD` 改为自己的强密码。如果通过域名访问，将 `RILL_BASE_URL` 设为公开地址；暂时只用 IP 访问时可以留空。保留 `RILL_VERSION=latest` 即使用最新稳定版：

```dotenv
RILL_USERNAME=admin
RILL_PASSWORD=替换为你自己的强密码
RILL_BASE_URL=
RILL_VERSION=latest
```

`RILL_PORT` 默认是 `8080`，端口已被占用时可在 `.env` 中修改。

**3. 拉取镜像并启动**

```bash
docker compose pull
docker compose up -d
```

访问 `http://服务器IP:8080/admin`，用 `.env` 中的用户名与密码登录；修改过 `RILL_PORT` 时使用对应端口。通过域名提供 HTTPS 访问的配置见下方[反向代理](#反向代理https)。

数据（SQLite 数据库与图片缓存）保存在 `./data`。容器以 `PUID`/`PGID`（默认 1000）运行，启动时会自动修正数据目录权限。

#### 更新在线镜像

在安装目录执行以下命令，即可拉取最新稳定版并更新容器：

```bash
docker compose pull
docker compose up -d
```

更新会保留 `.env` 和 `data/`，数据库会自动迁移；更新前可按下方[运维](#运维)说明备份数据库。

需要固定版本时，在 `.env` 设置 `RILL_VERSION=0.1.4`，再执行相同命令。可用版本见 [GitHub Releases](https://github.com/wakewon/RillRSS/releases)。以上部署与更新流程均无需下载源码、安装 Rust 或本地编译。

### 方式二：离线 Docker 安装包

服务器无法直接访问 GHCR，或需要先下载镜像再传到服务器时，可以使用 [GitHub Releases](https://github.com/wakewon/RillRSS/releases) 中的离线安装包。它们同样由 GitHub Actions 编译，包内包含 Docker 镜像、Compose 配置、安装 / 升级脚本和校验文件。

| 服务器架构 | 下载的安装包 |
|---|---|
| x86-64 / amd64 | `rillrss-版本号-linux-amd64.tar.gz` 及同名 `.sha256` 文件 |
| ARM64 / aarch64 | `rillrss-版本号-linux-arm64.tar.gz` 及同名 `.sha256` 文件 |

离线安装默认使用 `/opt/rillrss` 作为固定目录，后续通过上传新发布包并运行 `update.sh` 升级。完整的校验、安装、配置与升级步骤见 [deploy/DEPLOY.zh-CN.md](deploy/DEPLOY.zh-CN.md)。

### 常用环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `RILL_VERSION` | `latest` | 在线镜像的版本标签；可改为 `0.1.4` 等已发布版本 |
| `RILL_PORT` | `8080` | Docker Compose 对外提供服务的宿主机端口 |
| `RILL_DATA_PATH` | `./data` | Docker Compose 在宿主机保存数据库与图片缓存的目录 |
| `RILL_USERNAME` | `admin` | 管理与 API 用户名 |
| `RILL_PASSWORD` | 空 | 首次启动时的初始密码（之后在后台修改） |
| `RILL_BASE_URL` | 空 | 手机访问到的公开地址（如 `https://rss.example.com`），用于缓存图片的地址；留空则按请求的 Host / X-Forwarded-* 自动推断 |
| `RILL_TRUST_PROXY` | `true` | 信任反向代理的 X-Forwarded-* 头（只对来自本机或私网的连接生效；客户端地址取 X-Forwarded-For 从右数第一个公网地址，伪造的条目不影响登录限速） |
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

Nginx 需要传递转发头（并且**不要**对 API 路径做重定向——重定向会让 POST 请求丢失内容）。用 `location ^~ /`，站点里其他按扩展名匹配的正则规则（面板常带的 `\.(jpg|png|css|js)$`、`.php` 处理）就不会拦截缓存图片、订阅图标和 `.php` 结尾的阅读器接口：

```nginx
location ^~ / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    client_max_body_size 32m;
}
```

完整示例（含 HTTP 跳转与证书）见 [deploy/nginx-rillrss.conf](deploy/nginx-rillrss.conf)。

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

每个订阅的处理顺序：**全文提取 → 清理 / 题图 / 繁简转换 → 翻译 → 图片缓存 → 发布**。一篇文章只有在全部处理完成后才对阅读器可见，并在发布时才分配文章 ID，因此阅读器永远不会先缓存到未翻译的版本，基于 `since_id` / `ot` 的增量同步也不会漏掉晚完成的文章。

### 翻译

「设置 → 翻译引擎」中选择主引擎与回退引擎：

| 引擎 | 说明 |
|---|---|
| AI 翻译 | 任意 OpenAI 兼容接口，质量最好，可附加术语表与风格要求；需要 API Key，按用量计费 |
| Google 翻译 | Chrome 网页翻译所用的接口，免费、无需 Key；保留链接与格式效果很好，一篇文章通常一两次请求、一两秒完成 |
| 微软翻译 | Edge 浏览器翻译所用的接口，免费、无需 Key；普通句子效果不错，行内链接偶尔错位，适合作为回退 |

- **回退引擎**：主引擎翻译失败或被内容审核拒绝的段落交给回退引擎补译，文章说明中注明"N 段由某引擎补译"。主引擎整体失败时（接口错误、余额不足等），前几次尝试仍等待主引擎恢复，最后一次尝试改用回退引擎翻译整篇；主引擎已暂停（熔断）或未配置时，新文章直接交给回退引擎，不必排队等待。回退引擎也失败时才按失败策略处理。
- 推荐组合：AI 翻译为主、Google 翻译回退；不想付费时直接用 Google 翻译为主、微软翻译回退。
- Google 与微软翻译是浏览器扩展（沉浸式翻译、Read Frog、kiss-translator 等）普遍使用的公开接口，没有服务保证，接口变化或服务器 IP 被限流时会暂时失败；每个引擎有独立的并发、每分钟请求上限与熔断（默认 4 并发、每分钟 120 次）。各引擎的译文分别缓存。
- 「工具」页可以只用某一个引擎测试翻译，概览页分别显示主引擎与回退引擎的状态。

以下为 AI 翻译的处理细节（切分、校验、缓存同样用于 Google / 微软翻译）：

- 将 HTML 切分为块级段落，行内标签（链接、加粗、图片、代码等）替换为编号占位符 `<g1>…</g1>`、`<x2/>`，翻译后再按原始 DOM 还原，**模型不可能破坏 HTML 结构**，图片与链接不会丢失。
- 默认以 `<seg id="N">` 标签批量请求（输出被截断时仍可保留已完成的段落），也可切换为 JSON 格式。
- 逐段质量校验：缺失 / 空白、占位符丢失或错位、未翻译（含"半翻译"整句残留）、原文与译文混杂、重复循环、过短（截断）/过长、相邻段错位。
- 失败的段落拆小批次重试，直到单段；单段最多 3 次尝试；仍失败的保留原文并在后续重试中只补译缺失段落（段落级缓存）。
- 传输层：并发上限、每分钟请求数、指数退避并遵守 `Retry-After`、区分额度耗尽 / 鉴权错误；连续失败触发熔断，暂停期间文章排队而不占用资源。
- 错误分类：请求参数、模型名称、Key 或余额问题（HTTP 400/401/403/404 等）重试也不会成功，**第一次出现即暂停翻译**，概览页显示原因和需要检查的配置，保存设置后立即恢复，不会形成请求风暴。"输入过长"与"内容审核拒绝"（含 DeepSeek、OpenRouter、智谱 1301 的审核错误）会拆分批次，定位到单段后保留原文。被内容审核拒绝的段落重试也一样会被拒绝，所以当失败的只有这类段落时，文章立即以混合结果发布（能翻译的用译文，被拒的用原文，附说明），不再重试，"继续等待"策略下也一样。很短的请求也超长、输出为空或反复无法解析时，视为 max_tokens / 推理模型等配置问题并暂停。
- 熔断恢复：暂停到期后只放行一个试探请求，成功才恢复，避免恢复瞬间涌入大量请求；限流（429）与额度耗尽分开处理。
- Temperature 按两位小数发送（智谱 GLM 等接口拒绝更多位小数，例如 `0.20000000298023224`）。
- 段落缓存按模型、目标语言、附加提示词与附加请求参数区分：更换模型、修改术语表 / 风格提示或请求参数后重新处理，会重新翻译而不是沿用旧译文。
- 已是目标语言的文章自动跳过；推理模型的 `<think>` 内容会被剔除；支持附加提示词与附加请求参数（如 `{"enable_thinking": false}`）。
- 失败策略可选：超过尝试次数后发布原文（附说明），或继续等待（最长 72 小时）。

### 新闻更新

1. **识别同一新闻**：按规范化 GUID 匹配；GUID 变化但链接相同且旧 GUID 已不在订阅中时视为同一新闻（适配 GUID = md5(标题) 的订阅）；去除跟踪参数；BBC 这类 `url#N`（版面序号）GUID 自动合并；同一文档中多条目共用链接时不做合并。
2. **判断实质修改**：去除"N 小时前"等易变内容后，用词/汉字 3-gram 计算相似度与新增量；错别字修正、标题微调、版面变化不会触发新版本（阈值可调）。启用全文时，条目变化会重新提取原文确认；「页面追踪」模式还会在首次出现后按时间表（默认 30 分钟、1.5、4、10、24、48 小时）复查原文页面。
3. **发布新版本**：新版本使用新的文章 ID、标题前缀 `[更新]`、正文顶部附更新说明（含首发时间），发布时间使用更新时间以便出现在列表顶部；旧版标记为已读但**仍保留在列表中**（Read You、Newsboat 等客户端只能通过列表得知已读状态），收藏保持，并把收藏/标签/备注带到新版本。
4. **防止重复与覆盖**：冷却期内的多次修改合并为一次发布；版本数上限；只追踪首次出现后一定时长内的更新；正在处理的旧修订一旦被新修订取代，就永远不会发布（旧翻译不会覆盖新版）。

### 修改设置后重新处理已发布文章

新的处理设置只作用于之后抓取的文章。要让已发布的文章按新设置更新（例如开启翻译、更换模型、开启繁简转换），在订阅详情页「重新处理已发布文章」或「任务」页「批量重新处理」中选择范围：最近 10 / 50 / 200 / 1000 篇、最近 1 / 7 / 30 天或全部文章，可勾选"仅未读"。

- 移动端阅读器不会刷新已缓存的文章，所以结果以**新文章 ID** 发布；新版本继承旧版的已读与收藏状态和原发布时间，不加 `[更新]` 前缀，也不计入新闻版本数。已读的旧版本直接隐藏；未读的旧版本标为已读并保留，以便客户端同步到已读状态。
- 重新处理不会让文章变差：原文页面已失效时保留当前的全文版本，翻译失败时保留当前版本（任务页可重试）。正在等待发布的新闻更新不会被重新处理吞掉，重复点击也不会重复排队。
- 重新处理以低优先级排队，新抓到的文章总是先处理；进度可在「任务」页查看。
- 已翻译过且原文未变的段落会命中缓存，不会重复计费；范围越大耗时越长，启用翻译时会产生相应的 API 调用。
- 旧版本按「设置 → 保留与清理 → 旧版本保留天数」自动删除；数据库使用增量回收，删除后的空间会逐步还给文件系统（0.1.2 首次启动时会整理一次旧数据库，数据库较大时启动需要多等一会儿）。

---

## 运维

- **健康检查**：`GET /healthz`；镜像内置 `HEALTHCHECK`（`rillrss healthcheck`）。
- **备份**：运行中可执行 `docker compose exec rillrss entrypoint.sh backup /data/backup-$(date +%F).db`（SQLite 在线备份，得到一致的快照）；或停止容器后复制 `./data` 目录。OPML 导出也包含所有订阅与处理选项。
- **完整性检查**：`docker compose exec rillrss entrypoint.sh check-db`（输出 `ok` 表示正常）。
- **不要在容器运行时用宿主机上的工具直接打开数据库**：SQLite 的 WAL 模式依赖共享内存，宿主机与容器（尤其是 Docker Desktop 的虚拟机文件共享、NAS、网络文件系统）同时访问可能损坏数据库。请使用上面的备份命令。
- **升级**：在线镜像执行 `docker compose pull && docker compose up -d`（固定版本的实例先修改 `.env` 中的 `RILL_VERSION`）；离线安装包按[离线部署说明](deploy/DEPLOY.zh-CN.md#升级)更新。数据库会自动迁移。
- **重置密码**：`docker compose exec rillrss entrypoint.sh reset-password 新密码`（通过 `entrypoint.sh` 执行可保证以服务用户身份运行）。
- **清理**：每小时自动执行（文章保留天数、每订阅上限、旧版本、过期翻译缓存、无引用图片、日志），也可在「任务」页手动执行。收藏永不删除。默认文章与旧版本各保留 14 天、未再使用的译文保留 30 天；已保存过设置的实例沿用保存的值，在「设置 → 保留与清理」中修改。
- **图片缓存容量**：超过「缓存总上限」后，按文章从旧到新删除缓存图片（收藏文章的图片最后删除），这些文章改用原图地址，阅读器里已缓存的旧地址也会跳转到原图；新文章的图片照常缓存。
- **日志**：`docker compose logs -f rillrss`；后台「日志」页记录抓取、更新、翻译、发布等事件。

### 多架构镜像

版本 tag 触发 GitHub Actions，在 GitHub 的 amd64 / arm64 原生 runner 上分别编译并验证，再发布一个多架构镜像：

```bash
docker pull ghcr.io/wakewon/rillrss:0.1.4
```

同一地址支持 `linux/amd64`、`linux/arm64`。稳定版另有 `latest` 标签，预发布版（如 `0.1.5-rc.1`）只发布对应版本标签。离线 Docker 安装包与 SHA-256 校验文件同时附在 GitHub Release 中。

发布步骤、两个远端的推送配置与工作流重试方式见 [docs/RELEASING.md](docs/RELEASING.md)。

---

## 开发

日常提交和 PR 会自动运行 GitHub Actions CI（单元测试与集成测试）；打版本 tag 后会完成测试、Docker 编译和发布。本机只需编辑代码和推送，不会产生 `target/` 或 Docker 编译缓存。

确需本地调试时：

```bash
cargo build --release
RILL_DATA_DIR=./data RILL_PASSWORD=dev-password ./target/release/rillrss
cargo test            # 单元测试 + 集成测试（内置模拟订阅服务器与模拟 OpenAI 接口）
```

手动从源码构建 Docker 镜像：

```bash
docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build
```

文档：

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 架构、数据模型、处理流水线、协议兼容性决策
- [docs/PLAN.md](docs/PLAN.md) — 调研结论与开发计划
- [docs/TESTING.md](docs/TESTING.md) — 自动化测试与实际验证记录

## 已知限制

见 [docs/TESTING.md](docs/TESTING.md#已知限制)。

## 许可证

MIT。协议实现参考了 FreshRSS、FreshAPI、Tiny Tiny RSS、TT-RSS News+ 插件与多个开源客户端的公开行为，未复制其代码（这些项目为 AGPL/GPL 许可）。
