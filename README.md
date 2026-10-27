# YouTube Downloader

可自行架設的 YouTube 影音下載網站。透過繁體中文網頁介面貼上影片連結，即可選擇畫質或音訊格式、追蹤下載進度，並管理下載紀錄。

以 Rust 建置後端，搭配 yt-dlp 與 FFmpeg 處理影音，提供帳號系統、兩步驟驗證與管理後台，支援透過 Docker Compose 在 Linux 部署。

## 功能特色

- **影音下載**：支援 YouTube 與 YouTube Music，提供 MP4 影片及 MP3、M4A 音訊，畫質依來源可用格式提供。
- **下載管理**：顯示下載進度與歷史紀錄；單支影片最長 2 小時，檔案保留 24 小時。
- **帳號安全**：密碼採 Argon2id 雜湊，支援 Google Authenticator 相容的 TOTP 驗證器與一次性備援碼。
- **登入管理**：登入最長保留 30 天，閒置 7 天失效，並可登出其他裝置。
- **管理後台**：控制是否開放註冊，依使用者查看、下載與刪除檔案，保留刪除操作紀錄。
- **容器部署**：以非 root 帳號執行，透過持久資料 volume 保存資料庫、下載檔案與金鑰。

## 快速開始

主機需安裝 Docker Engine 與 Compose plugin。下載或 clone 本專案後，在專案目錄建立設定檔：

```sh
cp .env.example .env
```

編輯 `.env`，填入實際使用的公開 HTTPS 網域，不加結尾 `/`：

```dotenv
APP_ORIGIN=https://downloads.example.com
APP_PORT=3000
```

啟動服務：

```sh
docker compose up --build -d
docker compose logs --tail=50 youtube_downloader
```

建置會先執行 Rust 測試，再編譯正式版本。映像內含 Python 3.12、uv 鎖定版本的 yt-dlp、Node.js 24 與 FFmpeg / FFprobe，主機不需另外安裝這些工具。

### 環境變數

| 變數 | 用途 | 預設值 |
| --- | --- | --- |
| `APP_ORIGIN` | 使用者造訪的公開 HTTPS 網址，不加結尾 `/` | 必填 |
| `APP_PORT` | Docker 對外發布的主機連接埠 | `3000` |
| `APP_KEY` | 手動指定既有站台的原始金鑰 | 未設定時自動產生並保存 |

容器內部固定使用 `3000`。若要沿用主機的 `3002`，只需在 `.env` 設定：

```dotenv
APP_PORT=3002
```

修改後執行 `docker compose up -d` 即可套用。`APP_PORT` 控制主機連接埠；`APP_ORIGIN` 應填入使用者實際造訪的公開網址。

### HTTPS 反向代理

使用 Caddy 或 Nginx 為公開網域提供 HTTPS，再轉發至主機的 `APP_PORT`。以下範例適用於 Caddy 與服務位於同一台主機，且 `APP_PORT=3000`：

```caddyfile
downloads.example.com {
    reverse_proxy 127.0.0.1:3000
}
```

若設定 `APP_PORT=3002`，請將反向代理目的埠同步改成 `3002`。Docker 預設將此埠發布到主機所有網路介面；若代理在另一台主機或容器，請使用代理能連線的主機位址。

完成設定後，透過 `APP_ORIGIN` 指定的 HTTPS 網址開啟網站。

## 管理員設定

新站先註冊帳號，再於部署主機授予管理員權限：

```sh
docker compose exec youtube_downloader youtube_downloader admin grant you@example.com
```

登入後造訪 `/admin` 即可管理註冊開關與使用者檔案。一般註冊不會自動取得管理員權限。

撤銷管理員權限：

```sh
docker compose exec youtube_downloader youtube_downloader admin revoke you@example.com
```

角色變更立即生效，不需重啟服務。管理員刪除檔案後，原下載連結即失效。

## 更新與備份

取得新版原始碼後，重新建置並啟動：

```sh
docker compose up --build -d
```

SQLite、下載檔案、`app.key` 與刪除暫存皆保存在 Docker named volume `data`。為相容既有部署，Compose 中的實際 volume 名稱為 `harbor_harbor-data`。

首次啟動會自動產生金鑰，存放於 `/data/app.key`，檔案權限為 `0600`。重啟與重建容器會沿用同一把金鑰，金鑰內容不會寫入日誌。

備份時請停止服務以保持資料一致，保存整個 volume，尤其是 `app.key`；若手動設定 `APP_KEY`，也需保存原設定。`docker compose down -v` 會刪除資料 volume，請勿用於一般更新。

### 遷移既有資料

停止原站後，將完整 SQLite 資料（含存在的 WAL / SHM）、下載檔案及原金鑰移入目的 volume，並將資料擁有者設為 `10001:10001`。原站若手動設定 `APP_KEY`，請在新站 `.env` 沿用相同設定。

請勿以新金鑰搭配舊資料庫，或覆蓋已在使用的目的站資料。若資料庫已存在但缺少金鑰，服務會停止並要求提供原金鑰，以保護既有 TOTP 資料。

## 技術架構與開發

| 元件 | 技術 |
| --- | --- |
| 後端 | Rust、Axum、Tokio |
| 資料儲存 | SQLite |
| 前端 | HTML、CSS、JavaScript |
| 影音處理 | yt-dlp、FFmpeg / FFprobe |
| 部署 | Docker Compose |

原始碼位於 `src/`，網頁資源位於 `static/`；Rust 與 Python 依賴分別由 `Cargo.lock` 與 `uv.lock` 鎖定。

在 Linux 上安裝專案指定的 Rust 工具鏈後，可執行測試：

```sh
cargo test --locked
```

也可透過 Docker 執行建置驗證：

```sh
docker compose build
```

Docker 建置會先執行測試，失敗時停止建置。公開 HTTPS 與實際 YouTube 下載需於部署環境另外確認。

## 使用限制與授權

僅下載你擁有權利或已獲授權的內容。YouTube 可能因登入要求、地區或流量限制拒絕下載。

本專案採用 [MIT License](LICENSE)。
