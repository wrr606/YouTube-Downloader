use super::*;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS admins(user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value INTEGER NOT NULL CHECK(value IN (0,1)));
        INSERT OR IGNORE INTO settings VALUES('registration_open',1);
        CREATE TABLE IF NOT EXISTS admin_audit(id INTEGER PRIMARY KEY,actor INTEGER NOT NULL REFERENCES users(id),registration_open INTEGER NOT NULL,created INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS admin_file_audit(id INTEGER PRIMARY KEY,actor INTEGER NOT NULL REFERENCES users(id),job_id TEXT NOT NULL,owner INTEGER NOT NULL REFERENCES users(id),title TEXT NOT NULL,created INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS download_limits(id INTEGER PRIMARY KEY CHECK(id=1),stream_mb INTEGER NOT NULL,job_mb INTEGER NOT NULL,storage_mb INTEGER NOT NULL);
        INSERT OR IGNORE INTO download_limits VALUES(1,256,768,4096);
        CREATE TABLE IF NOT EXISTS download_limits_audit(id INTEGER PRIMARY KEY,actor INTEGER NOT NULL REFERENCES users(id),stream_mb INTEGER NOT NULL,job_mb INTEGER NOT NULL,storage_mb INTEGER NOT NULL,created INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS session_policy(id INTEGER PRIMARY KEY CHECK(id=1),hours INTEGER NOT NULL CHECK(hours BETWEEN 1 AND 8760));
        INSERT OR IGNORE INTO session_policy VALUES(1,720);
        CREATE TABLE IF NOT EXISTS session_policy_audit(id INTEGER PRIMARY KEY,actor INTEGER NOT NULL REFERENCES users(id),hours INTEGER NOT NULL,created INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS download_retention(id INTEGER PRIMARY KEY CHECK(id=1),file_hours INTEGER NOT NULL,history_hours INTEGER NOT NULL);
        INSERT OR IGNORE INTO download_retention VALUES(1,24,48);
        CREATE TABLE IF NOT EXISTS download_retention_audit(id INTEGER PRIMARY KEY,actor INTEGER NOT NULL REFERENCES users(id),file_hours INTEGER NOT NULL,history_hours INTEGER NOT NULL,created INTEGER NOT NULL);")
}

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPolicy {
    pub hours: u32,
}
impl SessionPolicy {
    fn validate(self) -> ApiResult<()> {
        if !(1..=8760).contains(&self.hours) {
            return Err(bad("登入有效時間須為 1–8760 小時"));
        }
        Ok(())
    }
    pub fn seconds(self) -> i64 {
        i64::from(self.hours) * 3600
    }
}
pub fn session_policy(db: &Connection) -> ApiResult<SessionPolicy> {
    let policy = db.query_row("SELECT hours FROM session_policy WHERE id=1", [], |r| {
        Ok(SessionPolicy { hours: r.get(0)? })
    })?;
    policy.validate()?;
    Ok(policy)
}
pub async fn session_settings(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    authorize(&a, &h, false)?;
    Ok(Json(json!(session_policy(&a.db.lock().unwrap())?)))
}
pub async fn update_session_settings(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(policy): Json<SessionPolicy>,
) -> ApiResult<Json<Value>> {
    let s = authorize(&a, &h, true)?;
    policy.validate()?;
    a.limit(format!("admin-session:{}", s.uid), 30, 60)?;
    let mut db = a.db.lock().unwrap();
    let tx = db.transaction()?;
    if !is_admin(&tx, s.uid)? {
        return Err(ApiError(StatusCode::FORBIDDEN, "此功能僅限管理員使用"));
    }
    tx.execute(
        "UPDATE session_policy SET hours=? WHERE id=1",
        [policy.hours],
    )?;
    tx.execute(
        "INSERT INTO session_policy_audit(actor,hours,created) VALUES(?,?,?)",
        params![s.uid, policy.hours, now()],
    )?;
    tx.commit()?;
    Ok(Json(json!(policy)))
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub file_hours: u32,
    pub history_hours: u32,
}
impl Retention {
    pub fn file_seconds(self) -> i64 {
        i64::from(self.file_hours) * 3600
    }
    pub fn history_seconds(self) -> i64 {
        i64::from(self.history_hours) * 3600
    }
    pub fn validate(self) -> ApiResult<()> {
        if !(1..=8760).contains(&self.file_hours) || !(2..=17520).contains(&self.history_hours) {
            return Err(bad(
                "檔案有效時間須為 1–8760 小時，紀錄保留時間須為 2–17520 小時",
            ));
        }
        if self.history_hours <= self.file_hours {
            return Err(bad("紀錄保留時間必須大於檔案有效時間"));
        }
        Ok(())
    }
}
pub fn retention(db: &Connection) -> ApiResult<Retention> {
    let policy = db.query_row(
        "SELECT file_hours,history_hours FROM download_retention WHERE id=1",
        [],
        |r| {
            Ok(Retention {
                file_hours: r.get(0)?,
                history_hours: r.get(1)?,
            })
        },
    )?;
    policy.validate()?;
    Ok(policy)
}
pub async fn retention_settings(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    authorize(&a, &h, false)?;
    Ok(Json(json!(retention(&a.db.lock().unwrap())?)))
}
pub async fn update_retention(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(policy): Json<Retention>,
) -> ApiResult<Json<Value>> {
    let s = authorize(&a, &h, true)?;
    policy.validate()?;
    a.limit(format!("admin-retention:{}", s.uid), 30, 60)?;
    // Do not let a policy extension race with removal under the previous policy.
    let _retention_guard = a.retention_lock.lock().await;
    let mut db = a.db.lock().unwrap();
    let tx = db.transaction()?;
    if !is_admin(&tx, s.uid)? {
        return Err(ApiError(StatusCode::FORBIDDEN, "此功能僅限管理員使用"));
    }
    tx.execute(
        "UPDATE download_retention SET file_hours=?,history_hours=? WHERE id=1",
        params![policy.file_hours, policy.history_hours],
    )?;
    tx.execute("INSERT INTO download_retention_audit(actor,file_hours,history_hours,created) VALUES(?,?,?,?)",params![s.uid,policy.file_hours,policy.history_hours,now()])?;
    tx.commit()?;
    Ok(Json(json!(policy)))
}

#[derive(Clone, Copy, Deserialize, serde::Serialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DownloadLimits {
    pub stream_mb: u32,
    pub job_mb: u32,
    pub storage_mb: u32,
}
impl DownloadLimits {
    pub fn stream_bytes(self) -> u64 {
        u64::from(self.stream_mb) * 1024 * 1024
    }
    pub fn job_bytes(self) -> u64 {
        u64::from(self.job_mb) * 1024 * 1024
    }
    pub fn storage_bytes(self) -> u64 {
        u64::from(self.storage_mb) * 1024 * 1024
    }
    pub fn validate(self) -> ApiResult<()> {
        if !(1..=10240).contains(&self.stream_mb)
            || !(1..=32768).contains(&self.job_mb)
            || !(1..=1048576).contains(&self.storage_mb)
        {
            return Err(bad(
                "串流上限須為 1–10240 MiB，處理上限 1–32768 MiB，總容量 1–1048576 MiB",
            ));
        }
        if self.job_mb < self.stream_mb {
            return Err(bad("單次處理上限不可小於單一串流上限"));
        }
        if self.storage_mb < 3 * self.job_mb {
            return Err(bad("總容量須至少為單次處理上限的 3 倍，以預留並行下載空間"));
        }
        Ok(())
    }
}
pub fn download_limits(db: &Connection) -> ApiResult<DownloadLimits> {
    let limits = db.query_row(
        "SELECT stream_mb,job_mb,storage_mb FROM download_limits WHERE id=1",
        [],
        |r| {
            Ok(DownloadLimits {
                stream_mb: r.get(0)?,
                job_mb: r.get(1)?,
                storage_mb: r.get(2)?,
            })
        },
    )?;
    limits.validate()?;
    Ok(limits)
}

#[cfg(test)]
mod limit_type_tests {
    use super::*;

    #[test]
    fn sqlite_limits_support_maximum_values_without_byte_overflow() {
        let limits = DownloadLimits {
            stream_mb: 10240,
            job_mb: 32768,
            storage_mb: 1048576,
        };
        limits.validate().unwrap();
        let db = Connection::open_in_memory().unwrap();
        let decoded = db
            .query_row(
                "SELECT ?1,?2,?3",
                params![limits.stream_mb, limits.job_mb, limits.storage_mb],
                |r| {
                    Ok(DownloadLimits {
                        stream_mb: r.get(0)?,
                        job_mb: r.get(1)?,
                        storage_mb: r.get(2)?,
                    })
                },
            )
            .unwrap();
        assert_eq!(limits, decoded);
        assert_eq!(decoded.stream_bytes(), 10_737_418_240_u64);
        assert_eq!(decoded.job_bytes(), 34_359_738_368_u64);
        assert_eq!(decoded.storage_bytes(), 1_099_511_627_776_u64);
    }
}

pub async fn limits(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    authorize(&a, &h, false)?;
    Ok(Json(json!(download_limits(&a.db.lock().unwrap())?)))
}
pub async fn update_limits(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(limits): Json<DownloadLimits>,
) -> ApiResult<Json<Value>> {
    let s = authorize(&a, &h, true)?;
    limits.validate()?;
    a.limit(format!("admin-limits:{}", s.uid), 30, 60)?;
    let mut db = a.db.lock().unwrap();
    let tx = db.transaction()?;
    if !is_admin(&tx, s.uid)? {
        return Err(ApiError(StatusCode::FORBIDDEN, "此功能僅限管理員使用"));
    }
    let working: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE status='working')",
        [],
        |r| r.get(0),
    )?;
    if working {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "目前有下載正在處理，請等完成後再修改大小限制",
        ));
    }
    tx.execute(
        "UPDATE download_limits SET stream_mb=?,job_mb=?,storage_mb=? WHERE id=1",
        params![limits.stream_mb, limits.job_mb, limits.storage_mb],
    )?;
    tx.execute("INSERT INTO download_limits_audit(actor,stream_mb,job_mb,storage_mb,created) VALUES(?,?,?,?,?)", params![s.uid,limits.stream_mb,limits.job_mb,limits.storage_mb,now()])?;
    tx.commit()?;
    Ok(Json(json!(limits)))
}
pub fn is_admin(db: &Connection, uid: i64) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM admins WHERE user_id=?)",
        [uid],
        |r| r.get(0),
    )
}
pub fn registration_open(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT value FROM settings WHERE key='registration_open'",
        [],
        |r| r.get(0),
    )
}
fn authorize(a: &App, h: &HeaderMap, write: bool) -> ApiResult<Session> {
    let s = session(a, h, write)?;
    if !is_admin(&a.db.lock().unwrap(), s.uid)? {
        return Err(ApiError(StatusCode::FORBIDDEN, "此功能僅限管理員使用"));
    }
    Ok(s)
}
pub async fn config(State(a): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let db = a.db.lock().unwrap();
    Ok(Json(
        json!({"registration_open":registration_open(&db)?,"download_limits":download_limits(&db)?,"retention":retention(&db)?,"session_policy":session_policy(&db)?}),
    ))
}
pub async fn page(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Html<&'static str>> {
    authorize(&a, &h, false)?;
    Ok(Html(include_str!("../static/admin.html")))
}
pub async fn settings(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    authorize(&a, &h, false)?;
    config(State(a)).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    registration_open: bool,
}
pub async fn update(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<Settings>,
) -> ApiResult<Json<Value>> {
    let s = authorize(&a, &h, true)?;
    a.limit(format!("admin:{}", s.uid), 30, 60)?;
    let mut db = a.db.lock().unwrap();
    // Recheck authority in the same transaction as the setting change.
    let tx = db.transaction()?;
    if !is_admin(&tx, s.uid)? {
        return Err(ApiError(StatusCode::FORBIDDEN, "此功能僅限管理員使用"));
    }
    tx.execute(
        "UPDATE settings SET value=? WHERE key='registration_open'",
        [c.registration_open],
    )?;
    tx.execute(
        "INSERT INTO admin_audit(actor,registration_open,created) VALUES(?,?,?)",
        params![s.uid, c.registration_open, now()],
    )?;
    tx.commit()?;
    Ok(Json(json!({"registration_open":c.registration_open})))
}

#[derive(Deserialize)]
pub struct UsersQuery {
    page: Option<u32>,
}
pub async fn users(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<UsersQuery>,
) -> ApiResult<Json<Value>> {
    authorize(&a, &h, false)?;
    let page = q.page.unwrap_or(1);
    if page == 0 || page > 10000 {
        return Err(bad("無效的頁碼"));
    }
    let db = a.db.lock().unwrap();
    let total: i64 = db.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    // Include accounts with zero downloads. Never select authentication secrets.
    let mut query = db.prepare(
        "SELECT u.id,u.email,EXISTS(SELECT 1 FROM admins a WHERE a.user_id=u.id),u.totp IS NOT NULL,
         (SELECT COUNT(*) FROM jobs j WHERE j.user_id=u.id AND j.status='ready' AND j.file IS NOT NULL AND j.created>?)
         FROM users u ORDER BY u.id DESC LIMIT 40 OFFSET ?",
    )?;
    let users = query
        .query_map(
            params![now() - retention(&db)?.file_seconds(), (page - 1) * 40],
            |r| {
                Ok(json!({
                    "id":r.get::<_,i64>(0)?,
                    "email":r.get::<_,String>(1)?,
                    "is_admin":r.get::<_,bool>(2)?,
                    "mfa":r.get::<_,bool>(3)?,
                    "ready_files":r.get::<_,i64>(4)?
                }))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(
        json!({"users":users,"total":total,"page":page,"page_size":40}),
    ))
}

#[derive(Deserialize)]
pub struct FilesQuery {
    user_id: Option<i64>,
    page: Option<u32>,
}
pub async fn files(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<FilesQuery>,
) -> ApiResult<Json<Value>> {
    authorize(&a, &h, false)?;
    let page = q.page.unwrap_or(1);
    if page == 0 || page > 10000 || q.user_id.is_some_and(|id| id <= 0) {
        return Err(bad("無效的篩選條件"));
    }
    let (mut files, users, total, policy) = {
        let db = a.db.lock().unwrap();
        let policy = retention(&db)?;
        let cutoff = now() - policy.file_seconds();
        let total:i64=db.query_row("SELECT COUNT(*) FROM jobs WHERE status='ready' AND file IS NOT NULL AND created>? AND (? IS NULL OR user_id=?)",params![cutoff,q.user_id,q.user_id],|r|r.get(0))?;
        let mut query=db.prepare("SELECT j.id,j.title,j.user_id,u.email,j.kind,j.quality,j.created,j.file FROM jobs j JOIN users u ON u.id=j.user_id WHERE j.status='ready' AND j.file IS NOT NULL AND j.created>? AND (? IS NULL OR j.user_id=?) ORDER BY j.created DESC,j.rowid DESC LIMIT 40 OFFSET ?")?;
        let files=query.query_map(params![cutoff,q.user_id,q.user_id,(page-1)*40],|r|Ok(json!({"id":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,"user_id":r.get::<_,i64>(2)?,"email":r.get::<_,String>(3)?,"kind":r.get::<_,String>(4)?,"quality":r.get::<_,String>(5)?,"created":r.get::<_,i64>(6)?,"file":r.get::<_,String>(7)?})))?.collect::<Result<Vec<_>,_>>()?;
        let mut query=db.prepare("SELECT u.id,u.email,COUNT(*) FROM jobs j JOIN users u ON u.id=j.user_id WHERE j.status='ready' AND j.file IS NOT NULL AND j.created>? GROUP BY u.id ORDER BY u.email LIMIT 1000")?;
        let users=query.query_map([cutoff],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"email":r.get::<_,String>(1)?,"count":r.get::<_,i64>(2)?})))?.collect::<Result<Vec<_>,_>>()?;
        (files, users, total, policy)
    };
    for file in &mut files {
        let path = media::checked_media_path(
            &a,
            file["id"].as_str().unwrap(),
            file["file"].as_str().unwrap(),
        );
        let size = match path {
            Ok(path) => tokio::fs::metadata(path)
                .await
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len()),
            Err(_) => None,
        };
        file["size_bytes"] = json!(size);
        file["available"] = json!(size.is_some());
        file.as_object_mut().unwrap().remove("file");
    }
    Ok(Json(
        json!({"files":files,"users":users,"page":page,"total":total,"page_size":40,"retention":policy}),
    ))
}
pub async fn file(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    authorize(&a, &h, false)?;
    let name: Option<String> = {
        let db = a.db.lock().unwrap();
        let cutoff = now() - retention(&db)?.file_seconds();
        db.query_row(
            "SELECT file FROM jobs WHERE id=? AND status='ready' AND created>?",
            params![id, cutoff],
            |r| r.get(0),
        )
        .optional()?
        .flatten()
    };
    media::serve_file(
        &a,
        &id,
        &name.ok_or(ApiError(StatusCode::NOT_FOUND, "檔案不存在或已過期"))?,
    )
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteFile {
    confirm: bool,
}
pub async fn delete_file(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(c): Json<DeleteFile>,
) -> ApiResult<Json<Value>> {
    let s = authorize(&a, &h, true)?;
    if !c.confirm {
        return Err(bad("請確認刪除檔案"));
    }
    if !media::valid_job_id(&id) {
        return Err(bad("無效的下載識別碼"));
    }
    a.limit(format!("admin-delete:{}", s.uid), 60, 60)?;
    let _retention_guard = a.retention_lock.lock().await;
    let delete_app = a.clone();
    tokio::task::spawn_blocking(move || {
        let a = delete_app;
        let mut db = a.db.lock().unwrap();
        let tx = db.transaction()?;
        if !is_admin(&tx, s.uid)? {
            return Err(ApiError(StatusCode::FORBIDDEN, "此功能僅限管理員使用"));
        }
        let row: Option<(String, Option<String>, i64, String)> = tx
            .query_row(
                "SELECT status,file,user_id,title FROM jobs WHERE id=?",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (status, name, owner, title) =
            row.ok_or(ApiError(StatusCode::NOT_FOUND, "找不到下載檔案"))?;
        if status == "deleted" {
            return Ok(Json(json!({"ok":true,"already_deleted":true})));
        }
        if status != "ready" {
            return Err(ApiError(StatusCode::CONFLICT, "只能刪除已完成的下載檔案"));
        }
        let name = name.ok_or(ApiError(StatusCode::NOT_FOUND, "檔案不存在"))?;
        let path = media::checked_media_path(&a, &id, &name)?;
        // Stage just the validated media file. Roll back the rename if the DB commit fails.
        let trash = a.data.join("trash");
        std::fs::create_dir_all(&trash).map_err(|_| bad("無法建立刪除暫存目錄"))?;
        let staged = trash.join(format!("{}-{}", id, random_hex(8)));
        std::fs::rename(&path, &staged)
            .map_err(|_| bad("無法移除檔案，請稍後重試或檢查目錄權限"))?;
        let commit = (|| -> ApiResult<()> {
            tx.execute(
                "UPDATE jobs SET status='deleted',file=NULL,error='檔案已由管理員刪除' WHERE id=?",
                [&id],
            )?;
            tx.execute(
                "INSERT INTO admin_file_audit(actor,job_id,owner,title,created) VALUES(?,?,?,?,?)",
                params![s.uid, id, owner, title, now()],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(error) = commit {
            let _ = std::fs::rename(&staged, &path);
            return Err(error);
        }
        // A failed unlink stays outside the download directory and is retried by cleanup.
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_dir(path.parent().unwrap());
        Ok(Json(json!({"ok":true})))
    })
    .await
    .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "刪除工作失敗"))?
}

pub fn set_role(db: &Connection, action: &str, address: &str) -> Result<(), String> {
    if !matches!(action, "grant" | "revoke") {
        return Err("用法：youtube_downloader admin grant|revoke email".into());
    }
    let uid: i64 = db
        .query_row("SELECT id FROM users WHERE email=?", [address], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or("找不到帳號；請先在網站註冊")?;
    if action == "grant" {
        db.execute("INSERT OR IGNORE INTO admins(user_id) VALUES(?)", [uid])
    } else {
        db.execute("DELETE FROM admins WHERE user_id=?", [uid])
    }
    .map_err(|e| e.to_string())?;
    Ok(())
}
pub fn cli(args: &[String]) -> Result<(), String> {
    if args.len() != 3 {
        return Err("用法：youtube_downloader admin grant|revoke email".into());
    }
    let address = email(&args[2]).map_err(|e| e.1.to_owned())?;
    let data = PathBuf::from(std::env::var("APP_DATA").unwrap_or("data".into()));
    let path = data.join("harbor.sqlite3");
    if !path.is_file() {
        return Err("找不到資料庫；請先啟動網站並建立帳號，或設定正確的 APP_DATA".into());
    }
    let db = Connection::open(path).map_err(|e| e.to_string())?;
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    db.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| e.to_string())?;
    initialize(&db).map_err(|e| e.to_string())?;
    set_role(&db, &args[1], &address)?;
    println!(
        "管理員權限已{}：{address}",
        if args[1] == "grant" {
            "授予"
        } else {
            "撤銷"
        }
    );
    Ok(())
}
