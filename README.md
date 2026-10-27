# YouTube_Downloader

Linux 上執行的 Rust YouTube 影音下載網站。以 Docker Compose 部署，Python 下載器由 uv.lock 固定版本；主機只需 Docker Engine 與 Compose plugin。

```text
YouTube_Downloader/
├── src/                  Rust 後端與安全測試
├── static/               網頁、樣式與 JavaScript
├── Cargo.toml
├── Cargo.lock
├── pyproject.toml         yt-dlp 依賴
├── uv.lock
├── Dockerfile
├── docker-compose.yml
├── .env.example
├── .dockerignore
├── .gitignore
└── README.md
```

## 啟動

```sh
cp .env.example .env
```

只需在 `.env` 設定實際網域，例如 `APP_ORIGIN=https://downloads.example.com`，不加結尾 `/`。首次啟動會自動產生亂數金鑰，保存於持久資料 volume 的 `/data/app.key`（權限 0600）；重啟、更新與重建容器都會繼續使用同一把金鑰，金鑰內容不會寫入日誌。公開網域無法由程式可靠推測，因此仍需設定 APP_ORIGIN。

若已在主機環境設定 APP_ORIGIN，也可直接執行啟動指令，不必建立 `.env`。例如 `APP_ORIGIN=https://downloads.example.com docker compose up --build -d`。

既有網站若使用手動 APP_KEY，請保留原設定；仍支援在 `.env` 設定 `APP_KEY=原金鑰`。若發現已有資料庫但缺少 app.key，程式會停止並要求原金鑰，不會自動產生新金鑰破壞原 TOTP 資料。

```sh
docker compose up --build -d
docker compose logs --tail=50 youtube_downloader
```

建置時會在 Linux 映像內執行 Rust 測試，再編譯正式版本。映像包含 Python 3.12、uv 鎖定的 yt-dlp、Node 24、FFmpeg / FFprobe，並以非 root 帳號執行。主機不用安裝 Rust、Python 或 npm。

服務將主機 `3002` 對映到容器 `3000`，並監聽主機所有網路介面。用 Caddy 或 Nginx 提供 `.env` 設定的 HTTPS 網域，反代理指向主機的 `3002` 埠；若反代理在另一個容器或另一台主機，請使用它能連到的主機 IP。Caddy 位於同一台主機時，設定如下：

```caddyfile
downloads.example.com {
    reverse_proxy 127.0.0.1:3002
}
```

## 管理員

新站先註冊帳號，再從 Linux 主機授予管理員權限：

```sh
docker compose exec youtube_downloader youtube_downloader admin grant you@example.com
```

撤銷時把 `grant` 換成 `revoke`。角色即刻生效，不需重啟；一般註冊不會自動取得管理員權限。

登入後進入 `/admin`，可設定是否開放註冊，查看各使用者的已完成下載、篩選使用者、下載與刪除檔案。刪除需要確認，會留下操作紀錄，原下載連結隨即失效。

## 資料保存

SQLite、下載檔案、app.key 與刪除暫存保存在 Docker named volume `data`（實際名稱保留 `harbor_harbor-data`，沿用既有資料）。更新程式使用 `docker compose up --build -d`。備份需包含整個 volume（尤其 app.key），手動指定 APP_KEY 時也需備份原設定；勿執行 `docker compose down -v`，這會移除資料 volume。

專案內的 `data/` 是原站資料，不會自動匯入 Docker volume。若要延續原帳號、管理員角色與 TOTP，需在原站停止後，將完整 SQLite（含存在的 WAL / SHM）及 downloads 匯入新的目的 volume，資料擁有者設為 `10001:10001`，並使用原 APP_KEY。不要用新金鑰搭配舊資料庫，也不要覆蓋已在使用的目的站。

## 保留功能

- 帳號密碼至少 8 字元，Argon2id 雜湊。
- Session 最長 30 天、閒置 7 天失效。
- Google Authenticator 相容 TOTP 與一次性備援碼。
- MP4 畫質選擇、MP3 / M4A 音訊與下載進度。
- 管理員註冊開關、使用者檔案管理及刪除稽核。
- 每支影片最長 2 小時，檔案保留 24 小時。

僅下載擁有權利或已獲授權的內容。YouTube 可能因登入、地區或流量限制拒絕下載。

## 驗證

本次僅重整 Linux 原始碼與部署設定，依要求未在 Windows 執行程式或測試。Linux 執行驗證請於目標主機執行 `docker compose build`，建置內的測試失敗時會停止。公開 HTTPS 與實際 YouTube 下載仍需在目標主機確認。

自動金鑰另有 Linux 測試，涵蓋首次生成、0600 權限、重啟後 TOTP 解密、同時初始化、明確指定金鑰，以及拒絕覆寫損毀或遺失的既有金鑰；會隨 Docker 建置執行，本次尚未在 Linux 實際執行。
