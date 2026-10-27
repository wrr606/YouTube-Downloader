use super::*;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn load_app_key(
    data: &std::path::Path,
    supplied: Option<&str>,
    supplied_file: Option<&std::path::Path>,
) -> Result<[u8; 32], String> {
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Write};
    use std::os::unix::fs::OpenOptionsExt;

    fn decode(value: &str) -> Result<[u8; 32], String> {
        data_encoding::HEXLOWER.decode(value.as_bytes()).ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| "APP_KEY 必須是 64 個小寫十六進位字元；既有金鑰不會自動覆寫".into())
    }
    fn read(path: &std::path::Path) -> Result<[u8; 32], String> {
        let value = std::fs::read_to_string(path)
            .map_err(|error| format!("無法讀取金鑰檔案：{error}"))?;
        decode(value.trim())
    }
    // Compose passes an empty APP_KEY when automatic management is requested.
    let supplied = supplied.filter(|value| !value.is_empty());
    let supplied_file = supplied_file.filter(|path| !path.as_os_str().is_empty());
    match (supplied, supplied_file) {
        (Some(_), Some(_)) => return Err("APP_KEY 與 APP_KEY_FILE 請擇一設定".into()),
        (Some(value), None) => return decode(value),
        (None, Some(path)) => return read(path),
        (None, None) => {}
    }
    let path = data.join("app.key");
    match std::fs::symlink_metadata(&path) {
        Ok(_) => return read(&path),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(format!("無法檢查金鑰檔案：{error}")),
    }
    // A missing secret on an existing installation must never silently rotate TOTP keys.
    if data.join("harbor.sqlite3").exists() {
        return Err("已有資料庫但缺少 app.key；請提供原 APP_KEY 或 APP_KEY_FILE，避免失去既有二階段驗證資料".into());
    }
    std::fs::create_dir_all(data).map_err(|error| format!("無法建立資料目錄：{error}"))?;
    let temporary = data.join(format!(".app-key-{}", random_hex(16)));
    let result = (|| -> Result<[u8; 32], String> {
        let mut output = OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(&temporary).map_err(|error| format!("無法建立金鑰：{error}"))?;
        output.write_all(random_hex(32).as_bytes())
            .and_then(|_| output.sync_all()).map_err(|error| format!("無法保存金鑰：{error}"))?;
        // Publish complete contents atomically without ever replacing a concurrent winner.
        match std::fs::hard_link(&temporary, &path) {
            Ok(()) => {
                File::open(data).and_then(|directory| directory.sync_all())
                    .map_err(|error| format!("無法同步金鑰目錄：{error}"))?;
                println!("已自動產生金鑰並保存至持久資料目錄的 app.key");
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("無法保存金鑰：{error}")),
        }
        read(&path)
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub fn constant(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0; n];
    OsRng.fill_bytes(&mut b);
    data_encoding::HEXLOWER.encode(&b)
}
pub fn digest(s: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(s.as_bytes()))
}
pub fn seal(key: &[u8; 32], plain: &[u8]) -> ApiResult<String> {
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), plain)
        .map_err(|_| bad("加密失敗"))?;
    let mut out = nonce.to_vec();
    out.extend(encrypted);
    Ok(data_encoding::HEXLOWER.encode(&out))
}
pub fn open(key: &[u8; 32], s: &str) -> ApiResult<Vec<u8>> {
    let b = data_encoding::HEXLOWER
        .decode(s.as_bytes())
        .map_err(|_| denied())?;
    if b.len() < 28 {
        return Err(denied());
    }
    Aes256Gcm::new_from_slice(key)
        .unwrap()
        .decrypt(Nonce::from_slice(&b[..12]), &b[12..])
        .map_err(|_| denied())
}
pub fn totp(secret: &[u8], step: i64) -> String {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret).unwrap();
    mac.update(&(step as u64).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let i = (h[19] & 15) as usize;
    let n = u32::from_be_bytes(h[i..i + 4].try_into().unwrap()) & 0x7fffffff;
    format!("{:06}", n % 1_000_000)
}
pub fn verify_totp(secret: &[u8], code: &str, t: i64, last: i64) -> Option<i64> {
    if code.len() != 6 || !code.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    [0, -1, 1]
        .into_iter()
        .map(|d| t / 30 + d)
        .find(|&s| s > last && constant(totp(secret, s).as_bytes(), code.as_bytes()))
}
pub fn youtube_url(raw: &str) -> ApiResult<String> {
    if raw.len() > 2048 {
        return Err(bad("網址太長"));
    }
    let u = url::Url::parse(raw.trim()).map_err(|_| bad("請貼上有效的 YouTube 網址"))?;
    if u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
    {
        return Err(bad("僅接受 HTTPS YouTube 影片網址"));
    }
    let id = match u.host_str() {
        Some("youtu.be") => u
            .path()
            .strip_prefix('/')
            .filter(|p| !p.contains('/'))
            .map(str::to_owned),
        Some("www.youtube.com" | "youtube.com" | "m.youtube.com" | "music.youtube.com") => {
            if u.path() == "/watch" {
                u.query_pairs()
                    .find(|(k, _)| k == "v")
                    .map(|(_, v)| v.into_owned())
            } else {
                u.path()
                    .strip_prefix("/shorts/")
                    .or_else(|| u.path().strip_prefix("/live/"))
                    .map(str::to_owned)
            }
        }
        _ => None,
    }
    .ok_or_else(|| bad("僅支援 YouTube 單一影片，請檢查網址"))?;
    if id.len() != 11
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(bad("無效的 YouTube 影片 ID"));
    }
    Ok(format!("https://www.youtube.com/watch?v={id}"))
}
