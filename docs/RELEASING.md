# 在线编译与发布

日常开发只需编辑和推送代码。编译、测试、Docker 镜像与离线安装包均由 GitHub Actions 完成；不需要先在本机生成二进制、`image.tar` 或 `dist/`。

## 发布流程

1. 更新 `Cargo.toml` 的 `package.version` 和 `Cargo.lock` 中 `rillrss` 包的版本号，保持一致。没有依赖变更时可直接编辑这两个版本字段，无需本地编译。
2. 提交代码，推送到 NAS 与 GitHub，等待 [CI](https://github.com/wakewon/RillRSS/actions/workflows/ci.yml) 通过。
3. 在该提交上创建带注释的版本 tag 并推送：

```bash
# 例：Cargo.toml 与 Cargo.lock 中的版本已改为 0.1.5 并提交。
git tag -a v0.1.5 -m 'Release 0.1.5'
git push origin main --follow-tags
```

[Release 工作流](https://github.com/wakewon/RillRSS/actions/workflows/release.yml) 校验 tag、Cargo.toml 和 Cargo.lock 的版本一致；不一致时立即失败。它会：

- 运行 Rust 格式检查、单元测试和集成测试，使用 `--locked` 保持依赖版本。
- 在 `ubuntu-24.04`（amd64）和 `ubuntu-24.04-arm`（arm64）上分别原生构建 Docker 镜像；依赖与 Docker 层缓存在 GitHub，本机不参与编译。
- 每种架构验证程序版本、服务启动、HTTP 健康检查、数据库完整性和在线备份。
- 生成并校验离线安装包，验证安装器能在对应架构上导入镜像并完成首次安装。
- 全部通过后，将两个架构合并为多架构镜像，并发布 GitHub Release。此前的中间镜像只有 digest，没有版本标签。

发布到 GHCR 使用自动生成的 `GITHUB_TOKEN`，创建 Release 也使用此 token，不需要 Docker Hub 账号或额外 Secrets。

## 发布产物

以 `v0.1.5` 为例：

| 产物 | 地址 / 文件名 |
|---|---|
| 多架构镜像 | `ghcr.io/wakewon/rillrss:0.1.5`、`:v0.1.5` |
| 当前稳定版镜像 | `ghcr.io/wakewon/rillrss:latest` |
| x86-64 离线 Docker 安装包 | `rillrss-0.1.5-linux-amd64.tar.gz` |
| ARM64 离线 Docker 安装包 | `rillrss-0.1.5-linux-arm64.tar.gz` |
| 下载校验文件 | 每个安装包对应的 `.tar.gz.sha256` |

安装包内包含 `image.tar`、Compose 配置、安装 / 升级脚本、部署说明与 `SHA256SUMS`。`ARCH` 文件记录架构，安装器会在导入镜像前检查与服务器架构一致。原来的 amd64 包仍可正常升级。

预发布 tag 使用 `v0.1.5-rc.1` 等格式。它会生成 GitHub Prerelease 和版本镜像，但不会更新 `latest`。发布 tag 不接受含 `+` 的构建元数据（Docker 标签不支持该字符）。稳定版发布会更新 `latest`，因此补发旧稳定版本时需注意该标签也会移动。

## NAS 与 GitHub 两个远端

本机保留 NAS 为 `origin` 的拉取源，并让 `origin` 同时向 NAS 与 GitHub 推送；另有 `github` 远端可用于查询或补推。配置保存在本机 `.git/config`，新 clone 需要单独配置：

```bash
git remote add github git@github.com:wakewon/RillRSS.git
git remote set-url --add --push origin ssh://gitea@git.stu.wieke.cn:33322/wwk7225/RillRSS.git
git remote set-url --add --push origin git@github.com:wakewon/RillRSS.git
git config push.followTags true
git remote -v
```

以上初始化命令只执行一次。如果远端或 push URL 已存在，不要重复添加。

之后 `git push origin main --follow-tags` 会向两个仓库推送 main 和可达的带注释 tag；也可以用 `git push origin tag v0.1.5` 仅推送某个 tag。两次远端推送不是原子事务，发生失败时分别检查 `git ls-remote origin` 与 `git ls-remote github`，再补推失败的远端。直接推 GitHub 可及时触发 Actions，无需等待 NAS 镜像同步。NAS 自动镜像仍应保持启用，避免它把 GitHub 的提交或 tag 覆盖为旧状态。

## 首次 GHCR 发布

GitHub 新建的容器包默认是 private，即使源仓库是公开的。首次发布后，在 [RillRSS 包设置](https://github.com/users/wakewon/packages/container/rillrss/settings) 中将 visibility 设为 **Public**，服务器才能匿名拉取镜像。这是包级别的一次性设置，后续版本沿用；离线安装包不受此设置影响。

依据：[GitHub 容器仓库文档](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry)、[包访问权限与可见性](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility)、[Docker 多架构构建文档](https://docs.docker.com/build/ci/github-actions/multi-platform/)。

## 查看进度与重试

```bash
gh run list --repo wakewon/RillRSS --workflow release.yml
gh run watch RUN_ID --repo wakewon/RillRSS --exit-status
gh run view RUN_ID --repo wakewon/RillRSS --log-failed
gh run rerun RUN_ID --repo wakewon/RillRSS --failed
```

也可在 Actions → Release → Run workflow 中填写已有 tag，或执行：

```bash
gh workflow run release.yml --repo wakewon/RillRSS -f tag=v0.1.5
```

手动发布仍构建指定 tag 对应的源码，并校验其版本，不会把 main 当作待发布版本。重试会更新同一 Release 的附件；失败留下的 draft 不会被发布为正式 Release。已经发布的 tag 应保持不变；需要修改应用代码时递增版本并打新 tag。

仅需在线验证日常代码时运行 CI 工作流即可，不需要创建 Release。源码 Docker 构建只作为显式选项保留在 `docker-compose.build.yml`，默认 Compose 始终拉取预构建镜像。
