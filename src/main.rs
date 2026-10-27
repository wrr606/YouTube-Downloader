mod admin;
mod media;
mod security;

use argon2::{
    Argon2, PasswordHasher, PasswordVerifier,
    password_hash::{PasswordHash, SaltString},
};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OptionalExtension, params};
use security::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

pub type ApiResult<T> = Result<T, ApiError>;
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        eprintln!("database: {e}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "伺服器暫時無法處理，請稍後重試",
        )
    }
}
pub fn bad(msg: &'static str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg)
}
pub fn denied() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "請重新登入，或檢查驗證資料")
}
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub struct App {
    pub db: Mutex<Connection>,
    pub key: [u8; 32],
    pub origin: String,
    pub secure: bool,
    pub data: PathBuf,
    pub ytdlp: String,
    pub workers: Arc<Semaphore>,
    pub hashes: Arc<Semaphore>,
    pub progress: Mutex<HashMap<String, media::JobProgress>>,
    pub retention_lock: tokio::sync::Mutex<()>,
    limits: Mutex<HashMap<String, (i64, u32)>>,
    dummy: String,
}
impl App {
    fn new(data: PathBuf, key: [u8; 32], origin: String, ytdlp: String) -> Self {
        std::fs::create_dir_all(data.join("downloads")).expect("create data directory");
        let db = Connection::open(data.join("harbor.sqlite3")).expect("open database");
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;
  CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, email TEXT UNIQUE NOT NULL, password TEXT NOT NULL, totp TEXT, pending TEXT, pending_until INTEGER, last_step INTEGER NOT NULL DEFAULT -1);
  CREATE TABLE IF NOT EXISTS sessions(token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), csrf TEXT NOT NULL, expires INTEGER NOT NULL, seen INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS recovery(user_id INTEGER NOT NULL REFERENCES users(id), code TEXT NOT NULL, PRIMARY KEY(user_id,code));
  CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), title TEXT NOT NULL, kind TEXT NOT NULL, quality TEXT NOT NULL, status TEXT NOT NULL, error TEXT, created INTEGER NOT NULL, file TEXT);
  CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
  CREATE INDEX IF NOT EXISTS jobs_user ON jobs(user_id,created);
  UPDATE jobs SET status='failed',error='伺服器重新啟動，請重新下載' WHERE status='working';").expect("schema");
        admin::initialize(&db).expect("admin schema");
        let dummy = hash_sync("dummy-password-with-no-account").unwrap();
        Self {
            db: Mutex::new(db),
            key,
            secure: origin.starts_with("https://"),
            origin,
            data,
            ytdlp,
            workers: Arc::new(Semaphore::new(3)),
            hashes: Arc::new(Semaphore::new(4)),
            progress: Mutex::new(HashMap::new()),
            retention_lock: tokio::sync::Mutex::new(()),
            limits: Mutex::new(HashMap::new()),
            dummy,
        }
    }
    pub fn limit(&self, key: String, max: u32, window: i64) -> ApiResult<()> {
        let t = now();
        let mut m = self.limits.lock().unwrap();
        m.retain(|_, v| v.0 > t);
        if m.len() >= 10000 && !m.contains_key(&key) {
            return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, "請稍後再試"));
        }
        let v = m.entry(key).or_insert((t + window, 0));
        if v.1 >= max {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "操作太頻繁，請稍後再試",
            ));
        }
        v.1 += 1;
        Ok(())
    }
    fn cookie(&self, token: &str, age: i64) -> String {
        format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            if self.secure {
                "__Host-harbor"
            } else {
                "harbor"
            },
            if self.secure { "; Secure" } else { "" }
        )
    }
}

#[derive(Clone)]
pub struct Session {
    pub uid: i64,
    pub token: String,
    pub csrf: String,
    pub email: String,
    pub mfa: bool,
}
fn session(a: &App, h: &HeaderMap, write: bool) -> ApiResult<Session> {
    let name = if a.secure {
        "__Host-harbor="
    } else {
        "harbor="
    };
    let raw = h
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| {
            s.split(';')
                .map(str::trim)
                .find_map(|x| x.strip_prefix(name))
        })
        .ok_or_else(denied)?;
    if raw.len() != 64 {
        return Err(denied());
    }
    let token = digest(raw);
    let db = a.db.lock().unwrap();
    let s=db.query_row("SELECT s.user_id,s.csrf,u.email,u.totp IS NOT NULL FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token=? AND s.expires>?",params![token,now()],|r|Ok(Session{uid:r.get(0)?,token:token.clone(),csrf:r.get(1)?,email:r.get(2)?,mfa:r.get(3)?})).optional()?.ok_or_else(denied)?;
    if write
        && !constant(
            s.csrf.as_bytes(),
            h.get("x-csrf-token")
                .map(|v| v.as_bytes())
                .unwrap_or_default(),
        )
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "安全驗證已過期，請重新整理頁面",
        ));
    }
    db.execute(
        "UPDATE sessions SET seen=? WHERE token=?",
        params![now(), token],
    )?;
    Ok(s)
}
fn issue(a: &App, uid: i64) -> ApiResult<Response> {
    let db = a.db.lock().unwrap();
    issue_locked(a, &db, uid)
}
fn issue_locked(a: &App, db: &Connection, uid: i64) -> ApiResult<Response> {
    let lifetime = admin::session_policy(db)?.seconds();
    let raw = random_hex(32);
    let csrf = random_hex(32);
    db.execute("DELETE FROM sessions WHERE expires<=?", [now()])?;
    // At most ten persistent sessions per account.
    db.execute("DELETE FROM sessions WHERE token IN (SELECT token FROM sessions WHERE user_id=? ORDER BY seen DESC LIMIT -1 OFFSET 9)",[uid])?;
    db.execute(
        "INSERT INTO sessions VALUES(?,?,?,?,?)",
        params![digest(&raw), uid, csrf, now() + lifetime, now()],
    )?;
    let mut r = Json(json!({"ok":true,"csrf":csrf})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&a.cookie(&raw, lifetime)).unwrap(),
    );
    Ok(r)
}
fn hash_sync(p: &str) -> ApiResult<String> {
    Argon2::default()
        .hash_password(p.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|v| v.to_string())
        .map_err(|_| bad("無法處理密碼"))
}
async fn password_work(a: &App, p: String, hash: Option<String>) -> ApiResult<String> {
    let _permit = a
        .hashes
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "系統忙碌，請稍後再試"))?;
    tokio::task::spawn_blocking(move || match hash {
        Some(h) => {
            let ok = PasswordHash::new(&h)
                .ok()
                .is_some_and(|h| Argon2::default().verify_password(p.as_bytes(), &h).is_ok());
            if ok { Ok(String::new()) } else { Err(denied()) }
        }
        None => hash_sync(&p),
    })
    .await
    .map_err(|_| bad("無法處理密碼"))?
}
#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
    #[serde(default)]
    code: String,
}
fn email(s: &str) -> ApiResult<String> {
    let e = s.trim().to_ascii_lowercase();
    if e.len() > 254
        || !e.is_ascii()
        || e.bytes().any(|c| c <= 32)
        || e.split('@').count() != 2
        || !e.split('@').all(|p| !p.is_empty())
    {
        return Err(bad("請輸入有效的電子郵件"));
    }
    Ok(e)
}
async fn register(State(a): State<Arc<App>>, Json(c): Json<Credentials>) -> ApiResult<Response> {
    if !admin::registration_open(&a.db.lock().unwrap())? {
        return Err(ApiError(StatusCode::FORBIDDEN, "目前不開放註冊"));
    }
    let e = email(&c.email)?;
    if c.password.chars().count() < 8 || c.password.len() > 128 {
        return Err(bad("密碼至少需要 8 個字元，且不得超過 128 個位元組"));
    }
    let h = password_work(&a, c.password, None).await?;
    let uid = {
        let db = a.db.lock().unwrap();
        // Recheck after password hashing so closing registration affects in-flight requests.
        if !admin::registration_open(&db)? {
            return Err(ApiError(StatusCode::FORBIDDEN, "目前不開放註冊"));
        }
        match db.execute(
            "INSERT INTO users(email,password) VALUES(?,?)",
            params![e, h],
        ) {
            Ok(_) => db.last_insert_rowid(),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(bad("無法建立帳號，請確認資料或改用登入"));
            }
            Err(e) => return Err(e.into()),
        }
    };
    issue(&a, uid)
}
async fn login(State(a): State<Arc<App>>, Json(c): Json<Credentials>) -> ApiResult<Response> {
    let e = email(&c.email)?;
    a.limit(format!("account:{}", digest(&e)), 10, 600)?;
    if c.password.len() > 128 || c.code.len() > 32 {
        return Err(denied());
    }
    let row = {
        a.db.lock()
            .unwrap()
            .query_row(
                "SELECT id,password,totp FROM users WHERE email=?",
                [e],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?
    };
    password_work(
        &a,
        c.password,
        Some(row.as_ref().map(|r| r.1.clone()).unwrap_or(a.dummy.clone())),
    )
    .await?;
    let (uid, _, _) = row.ok_or_else(denied)?;
    // Re-read MFA after the expensive password check, under the same lock as session issuance.
    // This prevents a concurrent enrollment from being bypassed by an in-flight login.
    let db = a.db.lock().unwrap();
    let secret: Option<String> =
        db.query_row("SELECT totp FROM users WHERE id=?", [uid], |r| r.get(0))?;
    if let Some(secret) = secret {
        if c.code.is_empty() {
            return Ok(Json(json!({"mfa_required":true})).into_response());
        }
        consume_factor(&a, &db, uid, &secret, &c.code)?;
    }
    issue_locked(&a, &db, uid)
}
async fn me(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, false)?;
    let is_admin = admin::is_admin(&a.db.lock().unwrap(), s.uid)?;
    Ok(Json(
        json!({"email":s.email,"mfa":s.mfa,"csrf":s.csrf,"is_admin":is_admin}),
    ))
}
async fn logout(State(a): State<Arc<App>>, h: HeaderMap) -> ApiResult<Response> {
    let s = session(&a, &h, true)?;
    a.db.lock()
        .unwrap()
        .execute("DELETE FROM sessions WHERE token=?", [s.token])?;
    let mut r = Json(json!({"ok":true})).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&a.cookie("", 0)).unwrap(),
    );
    Ok(r)
}
#[derive(Deserialize)]
struct FactorInput {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}
async fn reauth(a: &App, s: &Session, p: String) -> ApiResult<()> {
    a.limit(format!("reauth:{}", s.uid), 8, 600)?;
    if p.len() > 128 {
        return Err(denied());
    }
    let hash =
        a.db.lock()
            .unwrap()
            .query_row("SELECT password FROM users WHERE id=?", [s.uid], |r| {
                r.get(0)
            })?;
    password_work(a, p, Some(hash)).await?;
    Ok(())
}
async fn enroll(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<FactorInput>,
) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, true)?;
    reauth(&a, &s, c.password).await?;
    let db = a.db.lock().unwrap();
    let enabled: bool = db.query_row(
        "SELECT totp IS NOT NULL FROM users WHERE id=?",
        [s.uid],
        |r| r.get(0),
    )?;
    if enabled {
        return Err(bad("已啟用兩步驟驗證"));
    }
    let mut secret = [0u8; 20];
    OsRng.fill_bytes(&mut secret);
    let b32 = data_encoding::BASE32_NOPAD.encode(&secret);
    let uri = format!(
        "otpauth://totp/YouTube_Downloader:{}?secret={b32}&issuer=YouTube_Downloader&algorithm=SHA1&digits=6&period=30",
        url::form_urlencoded::byte_serialize(s.email.as_bytes()).collect::<String>()
    );
    let qr = qrcode::QrCode::new(uri.as_bytes())
        .map_err(|_| bad("無法產生 QR Code"))?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build();
    db.execute(
        "UPDATE users SET pending=?,pending_until=? WHERE id=?",
        params![seal(&a.key, &secret)?, now() + 600, s.uid],
    )?;
    use base64::Engine;
    Ok(Json(
        json!({"secret":b32,"qr":format!("data:image/svg+xml;base64,{}",base64::engine::general_purpose::STANDARD.encode(qr))}),
    ))
}
async fn confirm(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<FactorInput>,
) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, true)?;
    a.limit(format!("factor:{}", s.uid), 8, 600)?;
    let db = a.db.lock().unwrap();
    let (secret, expires): (Option<String>, Option<i64>) = db
        .query_row(
            "SELECT pending,pending_until FROM users WHERE id=? AND totp IS NULL",
            [s.uid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(denied)?;
    let secret = secret
        .filter(|_| expires.unwrap_or(0) > now())
        .ok_or_else(|| bad("設定已逾時，請重新開始"))?;
    let step = verify_totp(&open(&a.key, &secret)?, &c.code, now(), -1).ok_or_else(denied)?;
    let codes: Vec<String> = (0..8).map(|_| random_hex(8)).collect();
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> ApiResult<()> {
        db.execute(
            "UPDATE users SET totp=?,pending=NULL,pending_until=NULL,last_step=? WHERE id=?",
            params![secret, step, s.uid],
        )?;
        db.execute("DELETE FROM recovery WHERE user_id=?", [s.uid])?;
        for code in &codes {
            db.execute(
                "INSERT INTO recovery VALUES(?,?)",
                params![s.uid, digest(code)],
            )?;
        }
        db.execute(
            "DELETE FROM sessions WHERE user_id=? AND token!=?",
            params![s.uid, s.token],
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = db.execute_batch("ROLLBACK");
        return Err(e);
    }
    db.execute_batch("COMMIT")?;
    Ok(Json(json!({"ok":true,"recovery_codes":codes})))
}
fn consume_factor(a: &App, db: &Connection, uid: i64, secret: &str, code: &str) -> ApiResult<()> {
    if code.len() > 32 {
        return Err(denied());
    }
    let last: i64 = db.query_row("SELECT last_step FROM users WHERE id=?", [uid], |r| {
        r.get(0)
    })?;
    if let Some(step) = verify_totp(&open(&a.key, secret)?, code, now(), last) {
        db.execute(
            "UPDATE users SET last_step=? WHERE id=?",
            params![step, uid],
        )?;
        return Ok(());
    }
    if code.len() == 16
        && db.execute(
            "DELETE FROM recovery WHERE user_id=? AND code=?",
            params![uid, digest(code)],
        )? == 1
    {
        return Ok(());
    }
    Err(denied())
}
async fn disable(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<FactorInput>,
) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, true)?;
    reauth(&a, &s, c.password).await?;
    let db = a.db.lock().unwrap();
    let secret: Option<String> =
        db.query_row("SELECT totp FROM users WHERE id=?", [s.uid], |r| r.get(0))?;
    consume_factor(&a, &db, s.uid, &secret.ok_or_else(denied)?, &c.code)?;
    db.execute(
        "UPDATE users SET totp=NULL,pending=NULL,pending_until=NULL,last_step=-1 WHERE id=?",
        [s.uid],
    )?;
    db.execute("DELETE FROM recovery WHERE user_id=?", [s.uid])?;
    db.execute(
        "DELETE FROM sessions WHERE user_id=? AND token!=?",
        params![s.uid, s.token],
    )?;
    Ok(Json(json!({"ok":true})))
}
async fn revoke(
    State(a): State<Arc<App>>,
    h: HeaderMap,
    Json(c): Json<FactorInput>,
) -> ApiResult<Json<Value>> {
    let s = session(&a, &h, true)?;
    reauth(&a, &s, c.password).await?;
    a.db.lock().unwrap().execute(
        "DELETE FROM sessions WHERE user_id=? AND token!=?",
        params![s.uid, s.token],
    )?;
    Ok(Json(json!({"ok":true})))
}

async fn guard(State(a): State<Arc<App>>, req: Request, next: Next) -> Response {
    let unsafe_method = !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if unsafe_method
        && req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            != Some(a.origin.as_str())
    {
        return ApiError(StatusCode::FORBIDDEN, "不允許跨網站操作").into_response();
    }
    if req.uri().path().starts_with("/api/") {
        let ip = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(IpAddr::from([127, 0, 0, 1]));
        if let Err(e) = a.limit(format!("api:{ip}"), 240, 60) {
            return e.into_response();
        }
        if matches!(req.uri().path(), "/api/login" | "/api/register")
            && let Err(e) = a.limit(format!("auth:{ip}"), 20, 600)
        {
            return e.into_response();
        }
    }
    let mut r = next.run(req).await;
    for (k, v) in [
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: https://i.ytimg.com; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
        ("cache-control", "no-store"),
    ] {
        r.headers_mut().insert(
            axum::http::HeaderName::from_static(k),
            HeaderValue::from_static(v),
        );
    }
    if a.secure {
        r.headers_mut().insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    r
}
pub fn router(a: Arc<App>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route(
            "/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../static/app.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/app.js"),
                )
            }),
        )
        .route("/api/register", post(register))
        .route("/api/config", get(admin::config))
        .route("/admin", get(admin::page))
        .route("/api/admin/users", get(admin::users))
        .route(
            "/api/admin/download-limits",
            get(admin::limits).post(admin::update_limits),
        )
        .route(
            "/api/admin/retention",
            get(admin::retention_settings).post(admin::update_retention),
        )
        .route("/api/admin/files", get(admin::files))
        .route("/api/admin/files/{id}/file", get(admin::file))
        .route("/api/admin/files/{id}/delete", post(admin::delete_file))
        .route(
            "/api/admin/settings",
            get(admin::settings).post(admin::update),
        )
        .route(
            "/admin.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/admin.js"),
                )
            }),
        )
        .route("/api/login", post(login))
        .route("/api/me", get(me))
        .route("/api/logout", post(logout))
        .route("/api/mfa/enroll", post(enroll))
        .route("/api/mfa/confirm", post(confirm))
        .route("/api/mfa/disable", post(disable))
        .route(
            "/api/admin/session-policy",
            get(admin::session_settings).post(admin::update_session_settings),
        )
        .route("/api/sessions/revoke", post(revoke))
        .route("/api/inspect", post(media::inspect))
        .route("/api/jobs", get(media::list).post(media::create))
        .route("/api/jobs/{id}/file", get(media::file))
        .layer(DefaultBodyLimit::max(8192))
        .layer(middleware::from_fn_with_state(a.clone(), guard))
        .with_state(a)
}
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        if args[0] != "admin" {
            eprintln!("用法：youtube_downloader [admin grant|revoke email]");
            std::process::exit(2);
        }
        if let Err(e) = admin::cli(&args) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    let origin = std::env::var("APP_ORIGIN").unwrap_or("http://127.0.0.1:3000".into());
    let u = url::Url::parse(&origin).expect("APP_ORIGIN must be a URL");
    assert!(
        u.origin().ascii_serialization() == origin,
        "APP_ORIGIN must be an exact origin, without trailing slash"
    );
    let bind: SocketAddr = std::env::var("APP_BIND")
        .unwrap_or("127.0.0.1:3000".into())
        .parse()
        .expect("APP_BIND");
    assert!(
        origin.starts_with("https://")
            || (bind.ip().is_loopback()
                && matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
        "HTTP is permitted on loopback only; use HTTPS in production"
    );
    let data: PathBuf = std::env::var("APP_DATA").unwrap_or("data".into()).into();
    let key = match load_app_key(
        &data,
        std::env::var("APP_KEY").ok().as_deref(),
        std::env::var_os("APP_KEY_FILE")
            .as_deref()
            .map(std::path::Path::new),
    ) {
        Ok(key) => key,
        Err(error) => {
            eprintln!("啟動失敗：{error}");
            std::process::exit(1);
        }
    };
    let ytdlp = std::env::var("YTDLP_PATH").unwrap_or("yt-dlp".into());
    println!("Using yt-dlp: {ytdlp}");
    let a = Arc::new(App::new(data, key, origin.clone(), ytdlp));
    let cleanup = a.clone();
    tokio::spawn(async move {
        loop {
            media::cleanup(&cleanup).await;
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .expect("bind listener");
    println!("YouTube_Downloader is running at {origin}");
    axum::serve(
        listener,
        router(a).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("serve");
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} }
}

#[cfg(test)]
mod tests;
