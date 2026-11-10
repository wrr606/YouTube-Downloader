# YouTube Downloader
可自行架設的 YouTube 影音下載網站。透過繁體中文網頁介面貼上影片連結，即可選擇畫質或音訊格式、追蹤下載進度，並管理下載紀錄。

以 Rust 建置後端，搭配 yt-dlp 與 FFmpeg 處理影音，提供帳號系統、兩步驟驗證與管理後台，支援透過 Docker Compose 在 Linux 部署。

## 功能特色
- **影音下載**：支援 YouTube 與 YouTube Music，提供 MP4 影片及 MP3、M4A 音訊，畫質依來源可用格式提供。
- **下載管理**：顯示下載進度與歷史紀錄；單支影片最長 2 小時，檔案保留 24 小時。
- **帳號安全**：密碼採 Argon2id 雜湊，支援 Google Authenticator 相容的 TOTP 驗證器與一次性備援碼。
- **登入管理**：登入採固定期限，預設 720 小時（30 天）；管理頁面可設定 1–8760 小時，新登入立即套用，既有登入保留原到期時間。活動不會展延期限，也沒有閒置逾期，並可登出其他裝置。
- **管理後台**：控制是否開放註冊，依使用者查看、下載與刪除檔案，保留刪除操作紀錄。
- **容器部署**：以非 root 帳號執行，透過持久資料 volume 保存資料庫、下載檔案與金鑰。

## 使用 Docker Hub 映像部署

在 Linux 主機安裝 Docker 和 Docker Compose，並將網域的 HTTPS 反向代理指向主機的 HTTP 連接埠（預設為 `3000`）。以下使用 `wrr606/youtube-downloader:v1.0.0` 映像，內含 yt-dlp 和 FFmpeg。

### 1. 建立部署目錄與 Compose 設定

```sh
mkdir -p YouTube-Downloader
cd YouTube-Downloader
```

在此目錄建立 `docker-compose.yml`，填入以下內容。

```yaml
name: youtube-downloader

services:
  youtube_downloader:
    image: wrr606/youtube-downloader:v1.0.0
    container_name: youtube-downloader
    ports:
      - "${APP_PORT:-3000}:3000"
    environment:
      APP_KEY: ${APP_KEY:-}
      APP_ORIGIN: ${APP_ORIGIN:?Set APP_ORIGIN=https://your-domain in .env; APP_KEY is generated automatically}
    volumes:
      - data:/data
    restart: unless-stopped
    read_only: true
    tmpfs:
      - /tmp:size=128M,mode=1777
    cap_drop: [ALL]
    security_opt:
      - no-new-privileges:true
volumes:
  data:
    name: youtube-downloader-data
```

### 2. 設定網址並啟動網站

在同一目錄建立 `.env`，把範例網域換成你的網域。網址開頭使用 `https://`，結尾省略 `/`。

```dotenv
APP_ORIGIN=https://downloads.example.com
APP_PORT=3000
```

在部署目錄執行以下指令，從 Docker Hub 下載指定版本並啟動網站。

```sh
docker compose pull && docker compose up -d
```

啟動後，用瀏覽器開啟 `APP_ORIGIN` 的網址。若網站無法開啟，可執行 `docker compose logs --tail=50 youtube_downloader` 查看啟動訊息。

## 從原始碼在本機建置並部署

在已安裝 Git、Docker 和 Docker Compose 的 Linux 主機上下載原始碼。若已有原始碼，直接進入含有 `Dockerfile` 的專案根目錄。

```sh
git clone https://github.com/wrr606/YouTube-Downloader.git
cd YouTube-Downloader
```

開啟專案的 `docker-compose.yml`，將 `youtube_downloader` 服務中的 `image: wrr606/youtube-downloader:v1.0.0` 那一行替換為 `build: .`，保留相同縮排與其餘設定。`.` 表示使用專案根目錄作為建置內容，並讀取該目錄的 `Dockerfile`。

首次部署時，用 `cp -n .env.example .env` 建立設定檔。若只在這台主機測試，將 `.env` 設為：

```dotenv
APP_ORIGIN=http://127.0.0.1:3000
APP_PORT=3000
```

若透過反向代理對外提供服務，請將 `APP_ORIGIN` 設為實際的 HTTPS 網址。在專案根目錄執行：

```sh
docker compose up --build -d
```

啟動後用瀏覽器開啟 `APP_ORIGIN` 指定的網址。資料會寫入 `youtube-downloader-data` 資料卷。

## 第一次使用

在首頁點「登入 / 註冊」→「建立帳號」。登入後貼上影片連結，選擇格式與品質，再按「開始下載」。

設定管理者帳號：

```sh
docker compose exec youtube_downloader youtube_downloader admin grant you@example.com
```

用這個帳號開啟網站的 `/admin`，即可開關註冊、設定檔案保留時間及管理下載檔案。

## 如何更新

在現有部署的 `docker-compose.yml` 所在目錄操作，沿用原本的 `.env` 和 `youtube-downloader-data` 資料卷。

### 使用 Docker Hub 映像

到 [Docker Hub 的 Tags 頁面](https://hub.docker.com/r/wrr606/youtube-downloader/tags) 找到要部署的版本。在 `docker-compose.yml` 的 `image: wrr606/youtube-downloader:v1.0.0` 中，將 `v1.0.0` 換成該版本標籤。儲存後執行：

```sh
docker compose pull && docker compose up -d
```

### 使用本機原始碼建置

如果是從 Git 複製專案，在專案根目錄取得目前分支的更新：

```sh
git pull --ff-only
```

### 確認更新結果

```sh
docker compose ps youtube_downloader
docker compose logs --tail=50 youtube_downloader
```

確認服務狀態為 `Up`，再開啟 `.env` 中的 `APP_ORIGIN` 網址，登入並查看原有的下載紀錄。

## 備份資料

- **備份資料**：執行 `docker compose stop` 後，備份整個 Docker 資料卷 `youtube-downloader-data`，其中包含資料庫、下載檔案和自動產生的金鑰 `app.key`。若曾手動設定 `APP_KEY`，也要保存原值。備份完成後執行 `docker compose start`。
- **保留資料**：一般更新或停站時請勿執行 `docker compose down -v`，這會一併刪除資料卷。

本專案採用 [MIT 授權](LICENSE)。
