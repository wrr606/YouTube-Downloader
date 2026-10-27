use super::*;
use axum::body::Body;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};
use tokio_util::io::ReaderStream;

#[derive(Clone, serde::Serialize)]
pub struct JobProgress {
    pub phase: &'static str,
    pub percent: Option<f64>,
}
fn parse_progress(line: &str) -> Option<JobProgress> {
    let line = line.trim();
    if let Some(phase) = line.strip_prefix("harbor-stage:") {
        return Some(JobProgress {
            phase: match phase {
                "downloading" => "downloading",
                "processing" => "processing",
                "finalizing" => "finalizing",
                _ => return None,
            },
            percent: None,
        });
    }
    let parts: Vec<_> = line.strip_prefix("harbor-progress:")?.split('|').collect();
    if parts.len() != 5 || !matches!(parts[0], "downloading" | "finished") {
        return None;
    }
    let phase = match (parts[1], parts[2]) {
        ("none", a) if a != "none" => "audio",
        (v, _) if v != "none" && v != "NA" => "video",
        _ => "downloading",
    };
    let percent = if parts[0] == "finished" {
        Some(100.0)
    } else {
        parts[3]
            .parse::<f64>()
            .ok()
            .zip(parts[4].parse::<f64>().ok())
            .filter(|(done, total)| {
                done.is_finite() && total.is_finite() && *done >= 0.0 && *total > 0.0
            })
            .map(|(done, total)| (done / total * 100.0).clamp(0.0, 100.0))
    };
    Some(JobProgress { phase, percent })
}
fn storage_usage(root: &std::path::Path) -> u64 {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()))
        .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten())
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
fn command(a: &App) -> Command {
    let mut c = Command::new(&a.ytdlp);
    c.args([
        "--ignore-config",
        "--no-plugin-dirs",
        "--no-playlist",
        "--no-cache-dir",
        "--no-warnings",
        "--js-runtimes",
        "node",
        "--socket-timeout",
        "15",
        "--retries",
        "1",
        "--extractor-retries",
        "1",
        "--no-progress",
    ]);
    c.process_group(0);
    // Downloader/FFmpeg/Node never inherit application keys or unrelated service credentials.
    c.env_clear();
    for name in ["PATH", "TMPDIR", "HOME", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(name) {
            c.env(name, value);
        }
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    c
}
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: this is the dedicated group created for our child, never the service group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}
async fn run(
    mut cmd: Command,
    seconds: u64,
    folder: Option<(&std::path::Path, u64)>,
    progress: Option<(&App, &str)>,
) -> ApiResult<Vec<u8>> {
    let mut child = cmd
        .spawn()
        .map_err(|_| bad("無法啟動 yt-dlp，請確認已安裝且路徑正確"))?;
    let pid = child.id();
    let mut group = ProcessGroup(pid);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let work = async {
        let read = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut out_reader = BufReader::new(stdout.take(2 * 1024 * 1024 + 1));
            let mut err_reader = stderr.take(65537);
            let (o, e) = tokio::join!(
                async {
                    loop {
                        let start = out.len();
                        if out_reader.read_until(b'\n', &mut out).await? == 0 {
                            break;
                        }
                        if let Some((app, id)) = progress
                            && let Some(update) =
                                parse_progress(&String::from_utf8_lossy(&out[start..]))
                        {
                            app.progress.lock().unwrap().insert(id.into(), update);
                        }
                    }
                    Ok::<(), std::io::Error>(())
                },
                err_reader.read_to_end(&mut err)
            );
            o.map_err(|_| bad("無法讀取下載程序"))?;
            e.map_err(|_| bad("無法讀取下載程序"))?;
            if out.len() > 2 * 1024 * 1024 || err.len() > 65536 {
                return Err(bad("下載程序輸出超過限制"));
            }
            Ok((out, err))
        };
        let monitor = async {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some((dir, max_bytes)) = folder {
                    let mut entries = tokio::fs::read_dir(dir)
                        .await
                        .map_err(|_| bad("無法讀取下載目錄"))?;
                    let mut sum = 0;
                    while let Ok(Some(e)) = entries.next_entry().await {
                        sum += e.metadata().await.map(|m| m.len()).unwrap_or(0);
                    }
                    if sum > max_bytes {
                        return Err::<(), _>(bad(
                            "下載處理總量超過管理員設定上限，請降低品質或請管理員調整大小限制",
                        ));
                    }
                }
            }
        };
        tokio::select! {r=async {let (status,(out,err))=tokio::try_join!(async{child.wait().await.map_err(|_|bad("下載程序異常"))},read)?;if folder.is_some() && (size_limit_reported(&out) || size_limit_reported(&err)) {return Err(bad("影音串流超過管理員設定的大小上限，請降低品質或請管理員調整大小限制"));}if !status.success(){let message=download_error(&err);eprintln!("yt-dlp exited with {status}: {message}");return Err(bad(message));}Ok(out)}=>r,r=monitor=>{r?;unreachable!()}}
    };
    let result = tokio::time::timeout(Duration::from_secs(seconds), work)
        .await
        .unwrap_or_else(|_| Err(bad("處理時間超過限制，請稍後重試")));
    if result.is_err() {
        if let Some(pid) = pid {
            // The child starts its own process group; terminate FFmpeg too.
            // SAFETY: kill takes an integer process-group identifier and does not access memory.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    group.0 = None;
    result
}
fn size_limit_reported(output: &[u8]) -> bool {
    let text = String::from_utf8_lossy(output).to_ascii_lowercase();
    text.contains("max-filesize") && (text.contains("larger than") || text.contains("too large"))
}
fn download_error(stderr: &[u8]) -> &'static str {
    let e = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if (e.contains("ffmpeg") || e.contains("ffprobe"))
        && (e.contains("not found") || e.contains("not installed"))
    {
        "伺服器找不到 FFmpeg，請安裝並加入 PATH 後重新啟動網站"
    } else if e.contains("requested format is not available") {
        "選擇的影音格式目前無法使用，請重新取得影片資訊並選擇品質"
    } else if e.contains("403") || e.contains("forbidden") {
        "YouTube 拒絕影音串流（HTTP 403），請確認網站使用最新版 yt-dlp 與可用的 JavaScript 執行環境"
    } else if e.contains("429") || e.contains("too many requests") {
        "YouTube 暫時限制此伺服器的請求，請稍後再試"
    } else if e.contains("sign in") || e.contains("private video") || e.contains("age-restricted") {
        "此影片需要 YouTube 登入或額外驗證，目前無法下載"
    } else if e.contains("no space left") || e.contains("disk full") {
        "伺服器磁碟空間不足，請清理後再試"
    } else if e.contains("permission denied") || e.contains("access is denied") {
        "伺服器無法寫入下載目錄，請檢查目錄權限"
    } else {
        "下載工具執行失敗，請檢查 yt-dlp、Node 與 FFmpeg 設定後重試"
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    #[test]
    fn size_skip_reports_are_distinct_from_other_tool_errors() {
        assert!(size_limit_reported(
            b"[download] File is larger than max-filesize (500 bytes > 100 bytes). Aborting."
        ));
        assert!(!size_limit_reported(
            b"ERROR: requested format is not available"
        ));
        assert!(!size_limit_reported(b"ERROR: HTTP 403 Forbidden"));
    }
    #[tokio::test]
    async fn processing_monitor_uses_supplied_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("media.part"), [0u8; 128]).unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let error = run(cmd, 3, Some((dir.path(), 64)), None).await.unwrap_err();
        assert!(error.1.contains("下載處理總量超過管理員設定"));
    }
    #[test]
    fn progress_tracks_each_stream_and_handles_unknown_totals() {
        let video = parse_progress("harbor-progress:downloading|avc1|none|25|100").unwrap();
        assert_eq!(video.phase, "video");
        assert_eq!(video.percent, Some(25.0));
        let audio = parse_progress("harbor-progress:downloading|none|mp4a|10|100").unwrap();
        assert_eq!(audio.phase, "audio");
        assert_eq!(audio.percent, Some(10.0));
        assert_eq!(
            parse_progress("harbor-progress:finished|none|mp4a|10|NA")
                .unwrap()
                .percent,
            Some(100.0)
        );
        for total in ["NA", "0", "NaN", "inf", "-10"] {
            assert_eq!(
                parse_progress(&format!("harbor-progress:downloading|none|mp4a|10|{total}"))
                    .unwrap()
                    .percent,
                None
            );
        }
        assert_eq!(
            parse_progress("harbor-progress:downloading|avc1|none|200|100")
                .unwrap()
                .percent,
            Some(100.0)
        );
        assert!(parse_progress("harbor-progress:unknown|avc1|none|25|100").is_none());
        assert!(parse_progress("harbor-progress:bad").is_none());
        assert!(parse_progress("an unrelated title").is_none());
        assert_eq!(
            parse_progress("harbor-stage:processing").unwrap().phase,
            "processing"
        );
        assert_eq!(
            parse_progress("harbor-stage:processing").unwrap().percent,
            None
        );
    }
    #[test]
    fn tool_errors_are_actionable_without_exposing_external_urls() {
        assert!(download_error(b"ERROR: ffmpeg not found").contains("FFmpeg"));
        assert!(
            download_error(b"HTTP Error 403: Forbidden https://private.example/token")
                .contains("403")
        );
        assert!(
            !download_error(b"HTTP Error 403: Forbidden https://private.example/token")
                .contains("private.example")
        );
        assert!(download_error(b"Requested format is not available").contains("格式"));
        assert!(download_error(b"HTTP Error 429: Too many requests").contains("限制"));
    }
}
#[derive(Deserialize)]
pub struct InspectInput {
    url: String,
}
pub async fn inspect(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<InspectInput>,
) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, true)?;
    a.limit(format!("inspect:{}", s.uid), 12, 600)?;
    let url = youtube_url(&c.url)?;
    let _permit = a
        .workers
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "下載工作已滿，請稍候再試"))?;
    let mut cmd = command(&a);
    cmd.args(["--skip-download", "--dump-single-json", "--", &url]);
    let out = run(cmd, 60, None, None).await?;
    let v: Value = serde_json::from_slice(&out).map_err(|_| bad("無法解析影片資訊"))?;
    if v["is_live"] == true
        || v["duration"]
            .as_f64()
            .is_none_or(|d| d <= 0.0 || d > 7200.0)
    {
        return Err(bad("僅支援兩小時以內、已結束的影片"));
    }
    let formats = v["formats"]
        .as_array()
        .ok_or_else(|| bad("此影片沒有可下載的格式"))?;
    let mut heights: Vec<u64> = formats
        .iter()
        .filter(|f| {
            f["ext"] == "mp4"
                && f["vcodec"].as_str().is_some_and(|c| c != "none")
                && f["has_drm"] != true
        })
        .filter_map(|f| f["height"].as_u64())
        .filter(|&h| h > 0 && h <= 4320)
        .collect();
    heights.sort_unstable();
    heights.dedup();
    heights.reverse();
    let audio = formats.iter().any(|f| {
        f["acodec"].as_str().is_some_and(|c| c != "none")
            && f["vcodec"] == "none"
            && f["has_drm"] != true
    });
    let m4a = formats.iter().any(|f| {
        f["ext"] == "m4a"
            && f["acodec"].as_str().is_some_and(|c| c != "none")
            && f["has_drm"] != true
    });
    // MP4 merges require an M4A audio track; progressive MP4 is also accepted by yt-dlp.
    if !m4a {
        heights.retain(|&height| {
            formats.iter().any(|f| {
                f["height"].as_u64() == Some(height)
                    && f["ext"] == "mp4"
                    && f["acodec"].as_str().is_some_and(|c| c != "none")
            })
        });
    }
    let id = url.split("v=").nth(1).unwrap();
    let mut result = json!({"url":url,"title":v["title"].as_str().unwrap_or("YouTube 影片").chars().take(200).collect::<String>(),"channel":v["channel"].as_str().unwrap_or("YouTube"),"duration":v["duration"],"thumbnail":format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"),"heights":heights,"audio":audio,"m4a":m4a});
    {
        let db = a.db.lock().unwrap();
        db.execute_batch("CREATE TABLE IF NOT EXISTS previews(user_id INTEGER NOT NULL,url TEXT NOT NULL,metadata TEXT NOT NULL,expires INTEGER NOT NULL,PRIMARY KEY(user_id,url));")?;
        db.execute("DELETE FROM previews WHERE expires<?", [now()])?;
        db.execute(
            "INSERT OR REPLACE INTO previews VALUES(?,?,?,?)",
            params![s.uid, url, result.to_string(), now() + 1800],
        )?;
    }
    result["download_limits"] = json!(admin::download_limits(&a.db.lock().unwrap())?);
    Ok(Json(result))
}
#[derive(Deserialize)]
pub struct JobInput {
    url: String,
    kind: String,
    quality: String,
}
pub async fn create(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<JobInput>,
) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, true)?;
    a.limit(format!("download:{}", s.uid), 12, 3600)?;
    let url = youtube_url(&c.url)?;
    let permit = a
        .workers
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "下載工作已滿，請稍候再試"))?;
    let id = uuid::Uuid::new_v4().to_string();
    let (title, limits) = {
        let db = a.db.lock().unwrap();
        let limits = admin::download_limits(&db)?;
        let pending: i64 = db.query_row(
            "SELECT COUNT(*) FROM jobs WHERE user_id=? AND status='working'",
            [s.uid],
            |r| r.get(0),
        )?;
        if pending >= 2 {
            return Err(bad("每個帳號最多同時處理兩個下載"));
        }
        // Reserve all three worker slots using the current configured job limit.
        if storage_usage(&a.data.join("downloads")) + 3 * limits.job_bytes()
            > limits.storage_bytes()
        {
            return Err(bad(
                "下載總容量不足以預留處理空間，請清理舊檔案或請管理員調整總容量",
            ));
        }
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM jobs WHERE created>?",
            [now() - 86400],
            |r| r.get(0),
        )?;
        if count >= 100 {
            return Err(bad("伺服器今日下載額度已用完"));
        }
        let meta: Option<String> = db
            .query_row(
                "SELECT metadata FROM previews WHERE user_id=? AND url=? AND expires>?",
                params![s.uid, url, now()],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        let v: Value =
            serde_json::from_str(&meta.ok_or_else(|| bad("請先取得影片資訊，再選擇品質"))?)
                .map_err(|_| bad("請重新取得影片資訊"))?;
        let valid = match c.kind.as_str() {
            "video" => c.quality.parse::<u64>().ok().is_some_and(|n| {
                v["heights"]
                    .as_array()
                    .is_some_and(|h| h.contains(&json!(n)))
            }),
            "mp3" => v["audio"] == true && matches!(c.quality.as_str(), "128" | "192" | "320"),
            "m4a" => v["m4a"] == true && c.quality == "source",
            _ => false,
        };
        if !valid {
            return Err(bad("不支援此品質，請重新選擇"));
        }
        let title = v["title"].as_str().unwrap_or("YouTube").to_string();
        db.execute("INSERT INTO jobs(id,user_id,title,kind,quality,status,created) VALUES(?,?,?,?,?,'working',?)",params![id,s.uid,title,c.kind,c.quality,now()])?;
        (title, limits)
    };
    let aid = id.clone();
    a.progress.lock().unwrap().insert(
        id.clone(),
        JobProgress {
            phase: "preparing",
            percent: None,
        },
    );
    tokio::spawn(async move {
        let _permit = permit;
        let dir = a.data.join("downloads").join(&aid);
        let result = async {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|_| bad("無法建立下載目錄"))?;
            let mut cmd = command(&a);
            let output = dir.join("media.%(ext)s");
            cmd.arg("--output").arg(&output).args([
                "--no-simulate", "--newline", "--progress", "--progress-delta", "0.5",
                "--progress-template", "download:harbor-progress:%(progress.status)s|%(info.vcodec)s|%(info.acodec)s|%(progress.downloaded_bytes)s|%(progress.total_bytes,progress.total_bytes_estimate)s",
                "--progress-template", "postprocess:harbor-stage:processing",
                "--print", "before_dl:harbor-stage:downloading",
                "--print", "after_move:harbor-stage:finalizing",
                "--match-filter",
                "duration <= 7200 & !is_live",
                "--restrict-filenames",
                "--no-part",
                "--no-keep-video",
            ]);
            cmd.arg("--max-filesize").arg(limits.stream_bytes().to_string());
            match c.kind.as_str() {
                "video" => {
                    cmd.args([
                        "--format",
                        &format!(
                            "bv[ext=mp4][height<={}]+ba[ext=m4a]/b[ext=mp4][height<={}]",
                            c.quality, c.quality
                        ),
                        "--merge-output-format",
                        "mp4",
                    ]);
                }
                "mp3" => {
                    cmd.args([
                        "--format",
                        "ba",
                        "--extract-audio",
                        "--audio-format",
                        "mp3",
                        "--audio-quality",
                        &format!("{}K", c.quality),
                    ]);
                }
                _ => {
                    cmd.args(["--format", "ba[ext=m4a]"]);
                }
            }
            cmd.args(["--", &url]);
            run(cmd, 900, Some((&dir, limits.job_bytes())), Some((&a,&aid))).await?;
            let ext = if c.kind == "video" {
                "mp4"
            } else if c.kind == "mp3" {
                "mp3"
            } else {
                "m4a"
            };
            let file = format!("media.{ext}");
            let meta = tokio::fs::metadata(dir.join(&file))
                .await
                .map_err(|_| bad("下載工具未產生預期的影音檔案，請重新取得影片資訊並重試"))?;
            if meta.len() == 0 {
                return Err(bad("下載檔案為空或不完整，請重試"));
            }
            if meta.len() > limits.job_bytes() {
                return Err(bad("輸出檔案超過管理員設定的單次處理上限，請降低品質或調整大小限制"));
            }
            Ok(file)
        }
        .await;
        let failed = result.is_err();
        {
            let db = a.db.lock().unwrap();
            match result {
                Ok(file) => {
                    let _ = db.execute(
                        "UPDATE jobs SET status='ready',file=? WHERE id=?",
                        params![file, aid],
                    );
                }
                Err(e) => {
                    let _ = db.execute(
                        "UPDATE jobs SET status='failed',error=? WHERE id=?",
                        params![e.1, aid],
                    );
                }
            }
        }
        a.progress.lock().unwrap().remove(&aid);
        if failed {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
    });
    Ok(Json(json!({"id":id,"title":title})))
}
pub async fn list(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, false)?;
    let db = a.db.lock().unwrap();
    let policy = admin::retention(&db)?;
    let t = now();
    let expires = t - policy.file_seconds();
    let history = t - policy.history_seconds();
    // Enforce deadlines on every read, even between periodic cleanup runs.
    let mut q=db.prepare("SELECT id,title,kind,quality,CASE WHEN created<=?1 THEN 'expired' ELSE status END,CASE WHEN created<=?1 OR status='expired' THEN NULL ELSE error END,created FROM jobs WHERE user_id=?2 AND created>?3 ORDER BY created DESC,rowid DESC LIMIT 30")?;
    let mut jobs=q.query_map(params![expires,s.uid,history],|r|Ok(json!({"id":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"quality":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"error":r.get::<_,Option<String>>(5)?,"created":r.get::<_,i64>(6)?})))?.collect::<Result<Vec<_>,_>>()?;
    let progress = a.progress.lock().unwrap();
    for job in &mut jobs {
        let state = if job["status"] == "ready" {
            JobProgress {
                phase: "ready",
                percent: Some(100.0),
            }
        } else {
            progress
                .get(job["id"].as_str().unwrap())
                .cloned()
                .unwrap_or(JobProgress {
                    phase: "preparing",
                    percent: None,
                })
        };
        job["progress"] = json!(state);
    }
    Ok(Json(json!({"jobs":jobs,"retention":policy})))
}
pub async fn file(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let s = session(&a, &h, false)?;
    if !valid_job_id(&id) {
        return Err(bad("無效的下載識別碼"));
    }
    let file: Option<String> = {
        let db = a.db.lock().unwrap();
        let cutoff = now() - admin::retention(&db)?.file_seconds();
        db.query_row(
            "SELECT file FROM jobs WHERE id=? AND user_id=? AND status='ready' AND created>?",
            params![id, s.uid, cutoff],
            |r| r.get(0),
        )
        .optional()?
        .flatten()
    };
    let file = file.ok_or(ApiError(StatusCode::NOT_FOUND, "檔案不存在或已過期"))?;
    serve_file(&a, &id, &file).await
}
pub(crate) fn valid_job_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}
pub(crate) fn checked_media_path(a: &App, id: &str, file: &str) -> ApiResult<PathBuf> {
    if !valid_job_id(id) || !matches!(file, "media.mp4" | "media.mp3" | "media.m4a") {
        return Err(bad("無效的檔案"));
    }
    let missing = || ApiError(StatusCode::NOT_FOUND, "檔案不存在或已過期");
    let root = std::fs::canonicalize(a.data.join("downloads")).map_err(|_| missing())?;
    let dir = std::fs::canonicalize(root.join(id)).map_err(|_| missing())?;
    let path = std::fs::canonicalize(dir.join(file)).map_err(|_| missing())?;
    if dir != root.join(id)
        || path != dir.join(file)
        || std::fs::symlink_metadata(&path)
            .map_err(|_| missing())?
            .file_type()
            .is_symlink()
    {
        return Err(bad("無效的檔案路徑"));
    }
    Ok(path)
}
pub(crate) async fn serve_file(a: &App, id: &str, file: &str) -> ApiResult<Response> {
    let path = checked_media_path(a, id, file)?;
    let f = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "檔案不存在或已過期"))?;
    let len = f.metadata().await.map_err(|_| bad("無法讀取檔案"))?.len();
    let content_type = if file.ends_with(".mp4") {
        "video/mp4"
    } else if file.ends_with(".mp3") {
        "audio/mpeg"
    } else {
        "audio/mp4"
    };
    let mut response = Body::from_stream(ReaderStream::new(f)).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).unwrap(),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")).unwrap(),
    );
    Ok(response)
}
fn recover_trash(a: &App) {
    // Share the DB lock with deletion: no live transaction can lose its staged file.
    let db = a.db.lock().unwrap();
    let Ok(entries) = std::fs::read_dir(a.data.join("trash")) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.is_ascii()
            || name.len() != 53
            || !valid_job_id(&name[..36])
            || name.as_bytes()[36] != b'-'
            || !name[37..].bytes().all(|b| b.is_ascii_hexdigit())
            || !entry
                .file_type()
                .is_ok_and(|t| t.is_file() && !t.is_symlink())
        {
            continue;
        }
        let id = &name[..36];
        let row = db
            .query_row("SELECT status,file FROM jobs WHERE id=?", [id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional();
        match row {
            Ok(Some((status, Some(file)))) if status == "ready" => {
                // A crash before the commit leaves a ready row: recover instead of deleting.
                if !matches!(file.as_str(), "media.mp4" | "media.mp3" | "media.m4a") {
                    continue;
                }
                let Ok(root) = std::fs::canonicalize(a.data.join("downloads")) else {
                    continue;
                };
                let Ok(dir) = std::fs::canonicalize(root.join(id)) else {
                    continue;
                };
                if dir != root.join(id) {
                    continue;
                }
                let destination = dir.join(file);
                match std::fs::symlink_metadata(&destination) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let _ = std::fs::rename(entry.path(), destination);
                    }
                    Ok(m) if m.is_file() && !m.file_type().is_symlink() => {
                        let _ = std::fs::remove_file(entry.path());
                    }
                    _ => {}
                }
            }
            Ok(_) => {
                let _ = std::fs::remove_file(entry.path());
            }
            Err(_) => {} // Preserve data when the database cannot be read.
        }
    }
}
pub async fn cleanup(a: &App) {
    let _retention_guard = a.retention_lock.lock().await;
    recover_trash(a);
    let (expired, purge_before): (Vec<String>, i64) = {
        let db = a.db.lock().unwrap();
        let policy = match admin::retention(&db) {
            Ok(policy) => policy,
            Err(_) => return,
        };
        let t = now();
        let expires = t - policy.file_seconds();
        // Keep enough internal usage data for the existing 24-hour download quota.
        let purge_before = t - policy.history_seconds().max(86400);
        let mut q = match db.prepare("SELECT id FROM jobs WHERE created<=?1 AND status!='working' AND (status!='expired' OR file IS NOT NULL OR created<=?2)") {
            Ok(q) => q,
            Err(_) => return,
        };
        let ids = q
            .query_map(params![expires, purge_before], |r| r.get(0))
            .map(|r| r.filter_map(Result::ok).collect())
            .unwrap_or_default();
        (ids, purge_before)
    };
    for id in expired {
        if valid_job_id(&id) {
            if tokio::fs::remove_dir_all(a.data.join("downloads").join(&id))
                .await
                .is_err()
                && a.data.join("downloads").join(&id).exists()
            {
                continue;
            }
            let _ = a.db.lock().unwrap().execute(
                "UPDATE jobs SET status='expired',file=NULL,error=NULL WHERE id=?",
                [&id],
            );
            let _ = a.db.lock().unwrap().execute(
                "DELETE FROM jobs WHERE id=? AND created<=? AND status='expired'",
                params![id, purge_before],
            );
        }
    }
    // Expired session records and old job history are not retained indefinitely.
    let db = a.db.lock().unwrap();
    let _ = db.execute("DELETE FROM sessions WHERE expires<=?", [now()]);
}
