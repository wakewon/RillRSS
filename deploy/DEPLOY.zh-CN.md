# RillRSS x86-64 Docker 部署

RillRSS 安装在一个固定目录（下文用 `/opt/rillrss`）。该目录保存：
- `.env`：配置；
- `data/`：数据库和图片缓存；
- 可选的 `compose.override.yml`：自定义 Compose 配置。

升级只替换镜像、`compose.yml` 和几个辅助文件，上面这些都会保留。

服务器需要 Linux x86-64、Docker Engine、Docker Compose v2 和 Nginx。

## 安装

把发布包和同名的 `.sha256` 文件上传到服务器，在它们所在的目录执行（版本号按实际文件名填写）：

```sh
VERSION=0.1.2
sha256sum -c "rillrss-${VERSION}-linux-amd64.tar.gz.sha256"
tar -xzf "rillrss-${VERSION}-linux-amd64.tar.gz"
sudo "./rillrss-${VERSION}-linux-amd64/install.sh" --install-dir /opt/rillrss
rm -rf "rillrss-${VERSION}-linux-amd64"
```

首次安装会导入镜像并生成 `/opt/rillrss/.env`，但不会启动服务。编辑 `.env`，至少填写密码和公开地址：

```dotenv
RILL_PASSWORD=替换为强密码（可用 openssl rand -hex 16 生成）
RILL_BASE_URL=https://rss.example.com
RILL_PORT=8080
```

- 这两项留空时 Compose 会拒绝启动。
- 初始密码只在第一次启动时写入，之后在后台「账户」页修改。
- `RILL_PORT` 是服务器本机端口，只绑定 `127.0.0.1`，Nginx 的 `proxy_pass` 要用同一个端口。

启动并确认健康：

```sh
cd /opt/rillrss
sudo docker compose up -d
curl -fsS http://127.0.0.1:8080/healthz
```

## 配置 Nginx 与 HTTPS

参考 `/opt/rillrss/nginx-rillrss.conf.example`：

1. 复制到 Nginx 配置目录。
2. 替换域名和证书路径，把 `proxy_pass` 的端口改成 `RILL_PORT`。
3. 执行 `sudo nginx -t && sudo systemctl reload nginx`。

还没有证书时，先用 Certbot 签发：`sudo certbot --nginx -d rss.example.com`。签发前，域名需要已经解析到服务器，并且公网能访问 TCP 80。

如果是把反向代理加到已有站点（例如面板生成的配置）：
- 请保留 `location ^~ /` 的写法；
- 或者删掉站点里 `location ~ .*\.(gif|jpg|png|css|js)$`、`.php` 这类正则规则。

否则缓存图片、订阅图标和 `.php` 结尾的阅读器接口会被这些规则拦截成 404。

阅读器客户端请直接填写 HTTPS 地址。管理后台地址是 `https://你的域名/admin`。

## 升级

上传新的发布包（和它的 `.sha256` 文件），然后执行：

```sh
sudo /opt/rillrss/update.sh /path/to/rillrss-新版本-linux-amd64.tar.gz
```

更新器依次：
1. 校验压缩包（旁边有 `.sha256` 文件时）和包内文件；
2. 在旧容器仍运行时备份数据库到 `data/backups/`（保留最近 3 份）；
3. 导入新镜像，替换 `compose.yml`（旧文件另存为 `compose.yml.previous`）；
4. 重启服务，并等待健康检查通过。

失败时它会打印回滚命令和最近的日志。

## 回滚

改回上一个版本：

```sh
cd /opt/rillrss
sudo cp compose.yml.previous compose.yml
sudo docker compose up -d
```

如需同时恢复升级前的数据库：

```sh
cd /opt/rillrss
sudo docker compose down
sudo cp -p data/backups/rillrss-旧版本-时间.db data/rillrss.db
sudo rm -f data/rillrss.db-wal data/rillrss.db-shm
sudo docker compose up -d
```

## 自定义与备份

需要修改 Compose 配置时，写在 `/opt/rillrss/compose.override.yml`。它会与 `compose.yml` 自动合并，升级时不会被覆盖。例如：

```yaml
services:
  rillrss:
    mem_limit: 512m
```

随时在线备份数据库：

```sh
cd /opt/rillrss
sudo docker compose exec rillrss entrypoint.sh backup "/data/backup-$(date +%F).db"
```

查看状态和日志：

```sh
cd /opt/rillrss
sudo docker compose ps
sudo docker compose logs --tail=100 rillrss
```
