use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request as HttpRequest},
};
use tempfile::TempDir;

#[tokio::test]
async fn retention_settings_require_admin_validate_and_survive_reinitialization() {
    let (tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "retention@example.test").await;
    let route = "/api/admin/retention";
    let policy = json!({"file_hours":72,"history_hours":96});
    assert_eq!(
        request(&a, "GET", route, json!({}), "", "", None).await.0,
        401
    );
    assert_eq!(
        request(&a, "GET", route, json!({}), &cookie, "", None)
            .await
            .0,
        403
    );
    assert_eq!(post(&a, route, policy.clone(), &cookie, &csrf).await.0, 403);
    admin::set_role(&a.db.lock().unwrap(), "grant", "retention@example.test").unwrap();
    assert_eq!(post(&a, route, policy.clone(), &cookie, "").await.0, 403);
    assert_eq!(
        request(
            &a,
            "POST",
            route,
            policy.clone(),
            &cookie,
            &csrf,
            Some("https://evil.example")
        )
        .await
        .0,
        403
    );
    for invalid in [
        json!({"file_hours":0,"history_hours":48}),
        json!({"file_hours":24,"history_hours":24}),
        json!({"file_hours":24,"history_hours":12}),
        json!({"file_hours":8761,"history_hours":17521}),
    ] {
        assert_eq!(post(&a, route, invalid, &cookie, &csrf).await.0, 400);
    }
    assert_eq!(
        post(&a, route, policy.clone(), &cookie, &csrf).await.2,
        policy
    );
    let db = Connection::open(tmp.path().join("harbor.sqlite3")).unwrap();
    admin::initialize(&db).unwrap();
    let saved = admin::retention(&db).unwrap();
    assert_eq!(saved.file_seconds(), 72 * 3600);
    assert_eq!(saved.history_seconds(), 96 * 3600);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM download_retention_audit WHERE actor=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        request(&a, "GET", "/api/config", json!({}), "", "", None)
            .await
            .2["retention"],
        policy
    );
    let (id, path) = ready_file(&a, 1);
    a.db.lock()
        .unwrap()
        .execute(
            "UPDATE jobs SET created=? WHERE id=?",
            params![now() - 25 * 3600, id],
        )
        .unwrap();
    assert_eq!(
        request(
            &a,
            "GET",
            &format!("/api/jobs/{id}/file"),
            json!({}),
            &cookie,
            "",
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(&a, "GET", "/api/admin/files", json!({}), &cookie, "", None)
            .await
            .2["total"],
        1
    );
    media::cleanup(&a).await;
    assert!(path.exists());
    admin::set_role(&a.db.lock().unwrap(), "revoke", "retention@example.test").unwrap();
    assert_eq!(
        request(&a, "GET", route, json!({}), &cookie, "", None)
            .await
            .0,
        403
    );
}

#[tokio::test]
async fn default_retention_expires_at_24_hours_and_hides_at_48_before_cleanup() {
    let (_tmp, a) = fixture();
    let (cookie, _) = signup(&a, "retention@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "retention@example.test").unwrap();
    let t = now();
    let mut rows = Vec::new();
    for age in [23, 24, 25, 48, 49] {
        let (id, path) = ready_file(&a, 1);
        a.db.lock()
            .unwrap()
            .execute(
                "UPDATE jobs SET created=? WHERE id=?",
                params![t - age * 3600, id],
            )
            .unwrap();
        rows.push((id, path));
    }
    a.db.lock()
        .unwrap()
        .execute(
            "UPDATE jobs SET status='failed',error='old failure' WHERE id=?",
            [&rows[2].0],
        )
        .unwrap();
    let (_, _, response) = request(&a, "GET", "/api/jobs", json!({}), &cookie, "", None).await;
    let jobs = response["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 3);
    assert_eq!(
        response["retention"],
        json!({"file_hours":24,"history_hours":48})
    );
    for (index, (id, _)) in rows.iter().enumerate() {
        let job = jobs
            .iter()
            .find(|job| job["id"].as_str() == Some(id.as_str()));
        match index {
            0 => assert_eq!(job.unwrap()["status"], "ready"),
            1 | 2 => {
                let job = job.unwrap();
                assert_eq!(job["status"], "expired");
                assert!(job["error"].is_null());
            }
            _ => assert!(job.is_none()),
        }
    }
    assert_eq!(
        request(
            &a,
            "GET",
            &format!("/api/jobs/{}/file", rows[1].0),
            json!({}),
            &cookie,
            "",
            None
        )
        .await
        .0,
        404
    );
    assert_eq!(
        request(
            &a,
            "GET",
            &format!("/api/admin/files/{}/file", rows[1].0),
            json!({}),
            &cookie,
            "",
            None
        )
        .await
        .0,
        404
    );
    assert_eq!(
        request(&a, "GET", "/api/admin/files", json!({}), &cookie, "", None)
            .await
            .2["total"],
        1
    );
    media::cleanup(&a).await;
    assert!(rows[0].1.exists());
    for (_, path) in &rows[1..] {
        assert!(!path.exists());
    }
    let db = a.db.lock().unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row(
            "SELECT status,error FROM jobs WHERE id=?",
            [&rows[2].0],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        )
        .unwrap(),
        ("expired".into(), None)
    );
    assert!(
        db.query_row(
            "SELECT id FROM users WHERE email='retention@example.test'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .is_ok()
    );
}

#[tokio::test]
async fn short_retention_hides_history_without_resetting_daily_download_quota() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "retention@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "retention@example.test").unwrap();
    assert_eq!(
        post(
            &a,
            "/api/admin/retention",
            json!({"file_hours":1,"history_hours":2}),
            &cookie,
            &csrf
        )
        .await
        .0,
        200
    );
    let (id, path) = ready_file(&a, 1);
    a.db.lock()
        .unwrap()
        .execute(
            "UPDATE jobs SET created=? WHERE id=?",
            params![now() - 3 * 3600, id],
        )
        .unwrap();
    assert!(
        request(&a, "GET", "/api/jobs", json!({}), &cookie, "", None)
            .await
            .2["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    media::cleanup(&a).await;
    assert!(!path.exists());
    assert_eq!(
        a.db.lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE created>?",
                [now() - 86400],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    a.db.lock()
        .unwrap()
        .execute(
            "UPDATE jobs SET created=? WHERE id=?",
            params![now() - 25 * 3600, id],
        )
        .unwrap();
    media::cleanup(&a).await;
    assert_eq!(
        a.db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn download_limits_require_admin_csrf_validate_and_persist() {
    let (tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "limits@example.test").await;
    let route = "/api/admin/download-limits";
    let values = json!({"stream_mb":512,"job_mb":1536,"storage_mb":8192});
    assert_eq!(
        request(&a, "GET", route, json!({}), "", "", None).await.0,
        401
    );
    assert_eq!(
        request(&a, "GET", route, json!({}), &cookie, "", None)
            .await
            .0,
        403
    );
    assert_eq!(post(&a, route, values.clone(), &cookie, &csrf).await.0, 403);
    admin::set_role(&a.db.lock().unwrap(), "grant", "limits@example.test").unwrap();
    assert_eq!(post(&a, route, values.clone(), &cookie, "").await.0, 403);
    assert_eq!(
        request(
            &a,
            "POST",
            route,
            values.clone(),
            &cookie,
            &csrf,
            Some("https://evil.example")
        )
        .await
        .0,
        403
    );
    for invalid in [
        json!({"stream_mb":0,"job_mb":768,"storage_mb":4096}),
        json!({"stream_mb":10241,"job_mb":16384,"storage_mb":65536}),
        json!({"stream_mb":512,"job_mb":256,"storage_mb":4096}),
        json!({"stream_mb":256,"job_mb":768,"storage_mb":2000}),
    ] {
        assert_eq!(post(&a, route, invalid, &cookie, &csrf).await.0, 400);
    }
    assert_eq!(
        post(&a, route, values.clone(), &cookie, &csrf).await.2,
        values
    );
    let db = Connection::open(tmp.path().join("harbor.sqlite3")).unwrap();
    admin::initialize(&db).unwrap();
    let persisted = admin::download_limits(&db).unwrap();
    assert_eq!(persisted.stream_bytes(), 512 * 1024 * 1024);
    assert_eq!(persisted.job_bytes(), 1536 * 1024 * 1024);
    assert_eq!(persisted.storage_bytes(), 8192 * 1024 * 1024);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM download_limits_audit WHERE actor=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        request(&a, "GET", "/api/config", json!({}), "", "", None)
            .await
            .2["download_limits"],
        values
    );
    ready_file(&a, 1);
    a.db.lock()
        .unwrap()
        .execute("UPDATE jobs SET status='working'", [])
        .unwrap();
    assert_eq!(
        post(
            &a,
            route,
            json!({"stream_mb":1024,"job_mb":3072,"storage_mb":16384}),
            &cookie,
            &csrf
        )
        .await
        .0,
        409
    );
    assert_eq!(
        admin::download_limits(&a.db.lock().unwrap()).unwrap(),
        persisted
    );
}

#[tokio::test]
async fn configured_stream_limit_is_passed_to_downloader_and_skip_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let script = tmp.path().join("fake-yt-dlp");
    std::fs::write(&script, r#"#!/bin/sh
printf '%s\n' "$@" > "$(dirname "$0")/arguments"
printf '[download] File is larger than max-filesize (999999999 bytes > 536870912 bytes). Aborting.\n'
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let a = Arc::new(App::new(
        tmp.path().to_path_buf(),
        [42; 32],
        "http://127.0.0.1:3000".into(),
        script.to_string_lossy().into_owned(),
    ));
    let (cookie, csrf) = signup(&a, "limits@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "limits@example.test").unwrap();
    assert_eq!(
        post(
            &a,
            "/api/admin/download-limits",
            json!({"stream_mb":512,"job_mb":1536,"storage_mb":8192}),
            &cookie,
            &csrf
        )
        .await
        .0,
        200
    );
    let url = "https://www.youtube.com/watch?v=abcdefghijk";
    {
        let db = a.db.lock().unwrap();
        db.execute_batch("CREATE TABLE previews(user_id INTEGER,url TEXT,metadata TEXT,expires INTEGER,PRIMARY KEY(user_id,url));").unwrap();
        db.execute(
            "INSERT INTO previews VALUES(1,?,?,?)",
            params![
                url,
                json!({"title":"Synthetic test","heights":[720],"audio":true,"m4a":true})
                    .to_string(),
                now() + 600
            ],
        )
        .unwrap();
    }
    assert_eq!(
        post(
            &a,
            "/api/jobs",
            json!({"url":url,"kind":"video","quality":"720"}),
            &cookie,
            &csrf
        )
        .await
        .0,
        200
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (status, error): (String, Option<String>) =
                a.db.lock()
                    .unwrap()
                    .query_row("SELECT status,error FROM jobs", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .unwrap();
            if status != "working" {
                assert_eq!(status, "failed");
                assert!(error.unwrap().contains("影音串流超過管理員設定"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let args = std::fs::read_to_string(tmp.path().join("arguments")).unwrap();
    let args: Vec<_> = args.lines().collect();
    let position = args
        .iter()
        .position(|arg| *arg == "--max-filesize")
        .unwrap();
    assert_eq!(args[position + 1], "536870912");
}

#[tokio::test]
async fn admin_users_include_accounts_without_downloads_and_exclude_secrets() {
    let (_tmp, a) = fixture();
    let (admin, _) = signup(&a, "admin@example.test").await;
    let (owner, _) = signup(&a, "owner@example.test").await;
    signup(&a, "no-downloads@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "admin@example.test").unwrap();
    ready_file(&a, 2);
    let encrypted = seal(&a.key, b"private TOTP secret").unwrap();
    a.db.lock()
        .unwrap()
        .execute(
            "UPDATE users SET totp=?,pending='private pending secret' WHERE id=2",
            [&encrypted],
        )
        .unwrap();
    assert_eq!(
        request(&a, "GET", "/api/admin/users", json!({}), "", "", None)
            .await
            .0,
        401
    );
    assert_eq!(
        request(&a, "GET", "/api/admin/users", json!({}), &owner, "", None)
            .await
            .0,
        403
    );
    let (status, _, data) =
        request(&a, "GET", "/api/admin/users", json!({}), &admin, "", None).await;
    assert_eq!(status, 200);
    assert_eq!(data["total"], 3);
    let users = data["users"].as_array().unwrap();
    let empty = users
        .iter()
        .find(|u| u["email"] == "no-downloads@example.test")
        .unwrap();
    assert_eq!(empty["ready_files"], 0);
    assert_eq!(empty["is_admin"], false);
    let owner = users
        .iter()
        .find(|u| u["email"] == "owner@example.test")
        .unwrap();
    assert_eq!(owner["mfa"], true);
    assert_eq!(owner["ready_files"], 1);
    assert!(
        users
            .iter()
            .any(|u| u["email"] == "admin@example.test" && u["is_admin"] == true)
    );
    for user in users {
        assert_eq!(user.as_object().unwrap().len(), 5);
        for secret in [
            "password",
            "totp",
            "pending",
            "csrf",
            "token",
            "recovery_codes",
        ] {
            assert!(user.get(secret).is_none());
        }
    }
    assert!(!data.to_string().contains(&encrypted));
    assert_eq!(
        request(
            &a,
            "GET",
            "/api/admin/users?page=0",
            json!({}),
            &admin,
            "",
            None
        )
        .await
        .0,
        400
    );
    assert_eq!(
        request(
            &a,
            "GET",
            "/api/admin/users?page=10001",
            json!({}),
            &admin,
            "",
            None
        )
        .await
        .0,
        400
    );
    admin::set_role(&a.db.lock().unwrap(), "revoke", "admin@example.test").unwrap();
    assert_eq!(
        request(&a, "GET", "/api/admin/users", json!({}), &admin, "", None)
            .await
            .0,
        403
    );
}

#[tokio::test]
async fn admin_users_paginate_all_accounts_without_truncation() {
    let (_tmp, a) = fixture();
    let (admin, _) = signup(&a, "admin@example.test").await;
    {
        let db = a.db.lock().unwrap();
        admin::set_role(&db, "grant", "admin@example.test").unwrap();
        for index in 0..42 {
            db.execute(
                "INSERT INTO users(email,password) VALUES(?,?)",
                params![
                    format!("user-{index}@example.test"),
                    "test-only-unused-hash"
                ],
            )
            .unwrap();
        }
    }
    let (_, _, first) = request(&a, "GET", "/api/admin/users", json!({}), &admin, "", None).await;
    let (_, _, second) = request(
        &a,
        "GET",
        "/api/admin/users?page=2",
        json!({}),
        &admin,
        "",
        None,
    )
    .await;
    assert_eq!(first["total"], 43);
    assert_eq!(first["users"].as_array().unwrap().len(), 40);
    assert_eq!(second["users"].as_array().unwrap().len(), 3);
    let ids: std::collections::HashSet<_> = first["users"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["users"].as_array().unwrap())
        .map(|u| u["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 43);
}

#[test]
fn automatic_app_key_is_private_persistent_and_unique() {
    use std::os::unix::fs::PermissionsExt;
    let first = TempDir::new().unwrap();
    let data = first.path().join("data");
    let key = load_app_key(&data, Some(""), None).unwrap();
    assert_eq!(key, load_app_key(&data, None, None).unwrap());
    assert_eq!(
        std::fs::metadata(data.join("app.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let second = TempDir::new().unwrap();
    assert_ne!(key, load_app_key(second.path(), None, None).unwrap());
    let encrypted = seal(&key, b"existing TOTP secret").unwrap();
    assert_eq!(
        open(&load_app_key(&data, None, None).unwrap(), &encrypted).unwrap(),
        b"existing TOTP secret"
    );
}

#[test]
fn automatic_app_key_never_replaces_missing_or_damaged_existing_secrets() {
    let directory = TempDir::new().unwrap();
    let data = directory.path();
    std::fs::write(data.join("harbor.sqlite3"), b"existing database").unwrap();
    assert!(load_app_key(data, None, None).is_err());
    assert!(!data.join("app.key").exists());
    std::fs::write(data.join("app.key"), b"damaged").unwrap();
    assert!(load_app_key(data, None, None).is_err());
    assert_eq!(std::fs::read(data.join("app.key")).unwrap(), b"damaged");
}

#[test]
fn supplied_app_key_remains_supported_and_invalid_settings_fail() {
    let directory = TempDir::new().unwrap();
    let data = directory.path();
    let key = "ab".repeat(32);
    assert_eq!(load_app_key(data, Some(&key), None).unwrap(), [0xab; 32]);
    assert!(!data.join("app.key").exists());
    let path = data.join("original.key");
    std::fs::write(&path, format!("{key}\n")).unwrap();
    assert_eq!(load_app_key(data, None, Some(&path)).unwrap(), [0xab; 32]);
    assert!(load_app_key(data, Some(&key), Some(&path)).is_err());
    assert!(load_app_key(data, Some("invalid"), None).is_err());
    assert!(load_app_key(data, None, Some(&data.join("missing.key"))).is_err());
}

#[test]
fn concurrent_first_boot_uses_one_complete_app_key() {
    let directory = TempDir::new().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(6));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let data = directory.path().to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                load_app_key(&data, None, None).unwrap()
            })
        })
        .collect();
    let keys: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(keys.iter().all(|key| *key == keys[0]));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

fn ready_file(a: &App, uid: i64) -> (String, PathBuf) {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = a.data.join("downloads").join(&id);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("media.mp4");
    std::fs::write(&path, b"test media").unwrap();
    a.db.lock()
        .unwrap()
        .execute(
            "INSERT INTO jobs VALUES(?,?,'Test title','video','1080','ready',NULL,?,'media.mp4')",
            params![id, uid, now()],
        )
        .unwrap();
    (id, path)
}

#[tokio::test]
async fn admin_files_enforce_access_filters_and_audited_deletion() {
    let (_tmp, a) = fixture();
    let (admin, csrf) = signup(&a, "admin@example.test").await;
    let (owner, owner_csrf) = signup(&a, "owner@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "admin@example.test").unwrap();
    let (id, path) = ready_file(&a, 2);
    ready_file(&a, 1);
    let download = format!("/api/admin/files/{id}/file");
    let delete = format!("/api/admin/files/{id}/delete");
    for route in ["/api/admin/files", download.as_str()] {
        assert_eq!(
            request(&a, "GET", route, json!({}), "", "", None).await.0,
            401
        );
        assert_eq!(
            request(&a, "GET", route, json!({}), &owner, "", None)
                .await
                .0,
            403
        );
    }
    assert_eq!(
        post(&a, &delete, json!({"confirm":true}), &owner, &owner_csrf)
            .await
            .0,
        403
    );
    assert_eq!(
        post(&a, &delete, json!({"confirm":true}), &admin, "")
            .await
            .0,
        403
    );
    assert_eq!(
        request(
            &a,
            "POST",
            &delete,
            json!({"confirm":true}),
            &admin,
            &csrf,
            Some("https://evil.example")
        )
        .await
        .0,
        403
    );
    assert_eq!(
        post(&a, &delete, json!({"confirm":false}), &admin, &csrf)
            .await
            .0,
        400
    );
    let (_, _, list) = request(&a, "GET", "/api/admin/files", json!({}), &admin, "", None).await;
    assert_eq!(list["total"], 2);
    assert_eq!(list["users"].as_array().unwrap().len(), 2);
    assert!(list["files"][0].get("file").is_none());
    let (_, _, list) = request(
        &a,
        "GET",
        "/api/admin/files?user_id=2",
        json!({}),
        &admin,
        "",
        None,
    )
    .await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["files"][0]["email"], "owner@example.test");
    assert_eq!(list["files"][0]["size_bytes"], 10);
    assert_eq!(
        request(
            &a,
            "GET",
            "/api/admin/files?page=0",
            json!({}),
            &admin,
            "",
            None
        )
        .await
        .0,
        400
    );
    assert_eq!(
        request(&a, "GET", &download, json!({}), &admin, "", None)
            .await
            .0,
        200
    );
    assert_eq!(
        post(&a, &delete, json!({"confirm":true}), &admin, &csrf)
            .await
            .0,
        200
    );
    assert!(!path.exists());
    assert_eq!(
        request(&a, "GET", &download, json!({}), &admin, "", None)
            .await
            .0,
        404
    );
    assert_eq!(
        request(
            &a,
            "GET",
            &format!("/api/jobs/{id}/file"),
            json!({}),
            &owner,
            "",
            None
        )
        .await
        .0,
        404
    );
    assert_eq!(
        post(&a, &delete, json!({"confirm":true}), &admin, &csrf)
            .await
            .2["already_deleted"],
        true
    );
    let db = a.db.lock().unwrap();
    assert_eq!(
        db.query_row("SELECT status FROM jobs WHERE id=?", [&id], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "deleted"
    );
    assert_eq!(
        db.query_row("SELECT actor,owner,job_id FROM admin_file_audit", [], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap(),
        (1, 2, id)
    );
}

#[tokio::test]
async fn admin_file_delete_rolls_back_on_audit_failure_and_rejects_working_jobs() {
    let (_tmp, a) = fixture();
    let (admin, csrf) = signup(&a, "admin@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "admin@example.test").unwrap();
    let (id, path) = ready_file(&a, 1);
    let route = format!("/api/admin/files/{id}/delete");
    a.db.lock().unwrap().execute_batch("CREATE TRIGGER audit_failure BEFORE INSERT ON admin_file_audit BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    assert_eq!(
        post(&a, &route, json!({"confirm":true}), &admin, &csrf)
            .await
            .0,
        500
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"test media");
    assert_eq!(
        a.db.lock()
            .unwrap()
            .query_row("SELECT status FROM jobs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ready"
    );
    a.db.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER audit_failure; UPDATE jobs SET status='working';")
        .unwrap();
    assert_eq!(
        post(&a, &route, json!({"confirm":true}), &admin, &csrf)
            .await
            .0,
        409
    );
    assert!(path.exists());
    assert_eq!(
        post(
            &a,
            "/api/admin/files/not-a-uuid/delete",
            json!({"confirm":true}),
            &admin,
            &csrf
        )
        .await
        .0,
        400
    );
}

#[tokio::test]
async fn admin_files_paginate_and_hide_expired_downloads() {
    let (_tmp, a) = fixture();
    let (admin, _) = signup(&a, "admin@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "admin@example.test").unwrap();
    for _ in 0..41 {
        ready_file(&a, 1);
    }
    let (_, _, first) = request(&a, "GET", "/api/admin/files", json!({}), &admin, "", None).await;
    let (_, _, second) = request(
        &a,
        "GET",
        "/api/admin/files?page=2",
        json!({}),
        &admin,
        "",
        None,
    )
    .await;
    assert_eq!(first["total"], 41);
    assert_eq!(first["files"].as_array().unwrap().len(), 40);
    assert_eq!(second["files"].as_array().unwrap().len(), 1);
    assert_ne!(first["files"][0]["id"], second["files"][0]["id"]);
    a.db.lock()
        .unwrap()
        .execute("UPDATE jobs SET created=?", [now() - 86401])
        .unwrap();
    assert_eq!(
        request(&a, "GET", "/api/admin/files", json!({}), &admin, "", None)
            .await
            .2["total"],
        0
    );
}

#[test]
fn media_paths_reject_symlink_escape_on_linux() {
    let (_tmp, a) = fixture();
    // No account required for the filesystem validator.
    let id = uuid::Uuid::new_v4().to_string();
    let dir = a.data.join("downloads").join(&id);
    std::fs::create_dir(&dir).unwrap();
    let secret = a.data.join("secret.txt");
    std::fs::write(&secret, b"private").unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("media.mp4")).unwrap();
    assert!(media::checked_media_path(&a, &id, "media.mp4").is_err());
    assert!(media::checked_media_path(&a, &id, "../../secret.txt").is_err());
    std::fs::remove_file(dir.join("media.mp4")).unwrap();
    std::fs::remove_dir(&dir).unwrap();
    std::fs::write(a.data.join("media.mp4"), b"outside downloads").unwrap();
    std::os::unix::fs::symlink(&a.data, &dir).unwrap();
    assert!(media::checked_media_path(&a, &id, "media.mp4").is_err());
    assert!(secret.exists());
}

#[tokio::test]
async fn interrupted_admin_deletion_recovers_uncommitted_files_and_finishes_committed_deletion() {
    let (_tmp, a) = fixture();
    signup(&a, "owner@example.test").await;
    let (id, path) = ready_file(&a, 1);
    let trash = a.data.join("trash");
    std::fs::create_dir(&trash).unwrap();
    let staged = trash.join(format!("{id}-0123456789abcdef"));
    std::fs::rename(&path, &staged).unwrap();
    // Simulate a process exit between the media rename and the DB commit.
    media::cleanup(&a).await;
    assert_eq!(std::fs::read(&path).unwrap(), b"test media");
    assert!(!staged.exists());
    // A committed deletion must never be resurrected after a crash.
    std::fs::rename(&path, &staged).unwrap();
    a.db.lock()
        .unwrap()
        .execute("UPDATE jobs SET status='deleted',file=NULL", [])
        .unwrap();
    let unrelated = trash.join("do-not-delete.txt");
    std::fs::write(&unrelated, b"unrelated").unwrap();
    media::cleanup(&a).await;
    assert!(!path.exists());
    assert!(!staged.exists());
    assert!(unrelated.exists());
}

#[tokio::test]
async fn admin_settings_require_role_csrf_and_trusted_origin() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "admin-check@example.test").await;
    assert_eq!(
        request(&a, "GET", "/api/admin/settings", json!({}), "", "", None)
            .await
            .0,
        401
    );
    assert_eq!(
        request(&a, "GET", "/admin", json!({}), &cookie, "", None)
            .await
            .0,
        403
    );
    assert_eq!(
        post(
            &a,
            "/api/admin/settings",
            json!({"registration_open":false}),
            &cookie,
            &csrf
        )
        .await
        .0,
        403
    );
    admin::set_role(&a.db.lock().unwrap(), "grant", "admin-check@example.test").unwrap();
    let (status, _, me) = request(&a, "GET", "/api/me", json!({}), &cookie, "", None).await;
    assert_eq!(status, 200);
    assert_eq!(me["is_admin"], true);
    assert_eq!(
        request(&a, "GET", "/admin", json!({}), &cookie, "", None)
            .await
            .0,
        200
    );
    assert_eq!(
        post(
            &a,
            "/api/admin/settings",
            json!({"registration_open":false}),
            &cookie,
            ""
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(
            &a,
            "POST",
            "/api/admin/settings",
            json!({"registration_open":false}),
            &cookie,
            &csrf,
            Some("https://attacker.example")
        )
        .await
        .0,
        403
    );
    assert!(admin::registration_open(&a.db.lock().unwrap()).unwrap());
}

#[tokio::test]
async fn closing_registration_blocks_new_accounts_but_preserves_login() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "owner@example.test").await;
    admin::set_role(&a.db.lock().unwrap(), "grant", "owner@example.test").unwrap();
    assert_eq!(
        post(
            &a,
            "/api/admin/settings",
            json!({"registration_open":false}),
            &cookie,
            &csrf
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(&a, "GET", "/api/config", json!({}), "", "", None)
            .await
            .2["registration_open"],
        false
    );
    assert_eq!(
        post(
            &a,
            "/api/register",
            json!({"email":"new@example.test","password":"abcdefgh"}),
            "",
            ""
        )
        .await
        .0,
        403
    );
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("owner@example.test", ""),
            "",
            ""
        )
        .await
        .0,
        200
    );
    assert_eq!(
        post(
            &a,
            "/api/admin/settings",
            json!({"registration_open":true}),
            &cookie,
            &csrf
        )
        .await
        .0,
        200
    );
    assert_eq!(
        post(
            &a,
            "/api/register",
            json!({"email":"new@example.test","password":"abcdefgh","is_admin":true}),
            "",
            ""
        )
        .await
        .0,
        200
    );
    let db = a.db.lock().unwrap();
    assert!(!admin::is_admin(&db, 2).unwrap());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM admin_audit", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn revoking_admin_role_takes_effect_for_existing_session() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "revoke@example.test").await;
    {
        let db = a.db.lock().unwrap();
        admin::set_role(&db, "grant", "revoke@example.test").unwrap();
        admin::set_role(&db, "revoke", "revoke@example.test").unwrap();
    }
    assert_eq!(
        post(
            &a,
            "/api/admin/settings",
            json!({"registration_open":false}),
            &cookie,
            &csrf
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &cookie, "", None)
            .await
            .2["is_admin"],
        false
    );
}

#[tokio::test]
async fn registration_policy_and_roles_survive_restart_without_promoting_first_user() {
    let (tmp, a) = fixture();
    signup(&a, "persistent@example.test").await;
    {
        let db = a.db.lock().unwrap();
        assert!(!admin::is_admin(&db, 1).unwrap());
        admin::set_role(&db, "grant", "persistent@example.test").unwrap();
        db.execute(
            "UPDATE settings SET value=0 WHERE key='registration_open'",
            [],
        )
        .unwrap();
    }
    drop(a);
    let a = App::new(
        tmp.path().into(),
        [42; 32],
        "http://127.0.0.1:3000".into(),
        "yt-dlp".into(),
    );
    let db = a.db.lock().unwrap();
    assert!(!admin::registration_open(&db).unwrap());
    assert!(admin::is_admin(&db, 1).unwrap());
    assert!(admin::set_role(&db, "grant", "not-registered@example.test").is_err());
}

#[tokio::test]
async fn registration_accepts_eight_characters_but_rejects_seven() {
    let (_tmp, a) = fixture();
    assert_eq!(
        post(
            &a,
            "/api/register",
            json!({"email":"short@example.test","password":"abcdef7"}),
            "",
            ""
        )
        .await
        .0,
        400
    );
    let (status, _, _) = post(
        &a,
        "/api/register",
        json!({"email":"eight@example.test","password":"abcdef78"}),
        "",
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        post(
            &a,
            "/api/login",
            json!({"email":"eight@example.test","password":"abcdef78"}),
            "",
            ""
        )
        .await
        .0,
        200
    );
    // Length is measured in characters, not UTF-8 bytes.
    assert_eq!(
        post(
            &a,
            "/api/register",
            json!({"email":"unicode@example.test","password":"一二三四五六七"}),
            "",
            ""
        )
        .await
        .0,
        400
    );
}

#[tokio::test]
async fn expired_mfa_setup_cannot_enable_authentication() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "expiry@example.test").await;
    let (_, _, v) = post(
        &a,
        "/api/mfa/enroll",
        json!({"password":"correct horse battery staple"}),
        &cookie,
        &csrf,
    )
    .await;
    let secret = data_encoding::BASE32_NOPAD
        .decode(v["secret"].as_str().unwrap().as_bytes())
        .unwrap();
    a.db.lock()
        .unwrap()
        .execute("UPDATE users SET pending_until=?", [now() - 1])
        .unwrap();
    assert_eq!(
        post(
            &a,
            "/api/mfa/confirm",
            json!({"code":totp(&secret,now()/30)}),
            &cookie,
            &csrf
        )
        .await
        .0,
        400
    );
    let (_, _, me) = request(&a, "GET", "/api/me", json!({}), &cookie, "", None).await;
    assert_eq!(me["mfa"], false);
}

#[tokio::test]
async fn disabling_mfa_needs_password_and_unused_factor() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "disable@example.test").await;
    post(
        &a,
        "/api/mfa/enroll",
        json!({"password":"correct horse battery staple"}),
        &cookie,
        &csrf,
    )
    .await;
    let secret = {
        let db = a.db.lock().unwrap();
        let s: String = db
            .query_row("SELECT pending FROM users", [], |r| r.get(0))
            .unwrap();
        open(&a.key, &s).unwrap()
    };
    let (_, _, v) = post(
        &a,
        "/api/mfa/confirm",
        json!({"code":totp(&secret,now()/30)}),
        &cookie,
        &csrf,
    )
    .await;
    let recovery = v["recovery_codes"][0].as_str().unwrap();
    assert_eq!(
        post(
            &a,
            "/api/mfa/disable",
            json!({"password":"wrong","code":recovery}),
            &cookie,
            &csrf
        )
        .await
        .0,
        401
    );
    assert_eq!(
        post(
            &a,
            "/api/mfa/disable",
            json!({"password":"correct horse battery staple","code":"bad"}),
            &cookie,
            &csrf
        )
        .await
        .0,
        401
    );
    assert_eq!(
        post(
            &a,
            "/api/mfa/disable",
            json!({"password":"correct horse battery staple","code":recovery}),
            &cookie,
            &csrf
        )
        .await
        .0,
        200
    );
    let (_, _, v) = request(&a, "GET", "/api/me", json!({}), &cookie, "", None).await;
    assert_eq!(v["mfa"], false);
    assert_eq!(
        a.db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM recovery", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("disable@example.test", ""),
            "",
            ""
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn user_cannot_inject_format_or_command_arguments() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "formats@example.test").await;
    a.db.lock().unwrap().execute_batch("CREATE TABLE previews(user_id INTEGER,url TEXT,metadata TEXT,expires INTEGER,PRIMARY KEY(user_id,url));").unwrap();
    let url = "https://www.youtube.com/watch?v=abcdefghijk";
    a.db.lock()
        .unwrap()
        .execute(
            "INSERT INTO previews VALUES(1,?,?,?)",
            params![
                url,
                json!({"title":"a test","heights":[720],"audio":true,"m4a":true}).to_string(),
                now() + 600
            ],
        )
        .unwrap();
    for (kind, quality) in [
        ("video", "720;calc"),
        ("--exec", "720"),
        ("mp3", "0 --exec calc"),
        ("m4a", "../../etc/passwd"),
        ("video", "2160"),
    ] {
        assert_eq!(
            post(
                &a,
                "/api/jobs",
                json!({"url":url,"kind":kind,"quality":quality}),
                &cookie,
                &csrf
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(a.workers.available_permits(), 3);
    assert_eq!(
        a.db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
use tower::ServiceExt;

fn fixture() -> (TempDir, Arc<App>) {
    let tmp = TempDir::new().unwrap();
    let a = Arc::new(App::new(
        tmp.path().to_path_buf(),
        [42; 32],
        "http://127.0.0.1:3000".into(),
        "missing-yt-dlp-test-program".into(),
    ));
    (tmp, a)
}
async fn request(
    a: &Arc<App>,
    method: &str,
    path: &str,
    body: Value,
    cookie: &str,
    csrf: &str,
    origin: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut r = HttpRequest::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .header("content-type", "application/json");
    if !cookie.is_empty() {
        r = r.header("cookie", cookie);
    }
    if !csrf.is_empty() {
        r = r.header("x-csrf-token", csrf);
    }
    if let Some(o) = origin {
        r = r.header("origin", o);
    }
    let r = router(a.clone())
        .oneshot(
            r.body(if method == "GET" {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = to_bytes(r.into_body(), 4 * 1024 * 1024).await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, headers, v)
}
async fn post(
    a: &Arc<App>,
    path: &str,
    body: Value,
    cookie: &str,
    csrf: &str,
) -> (StatusCode, HeaderMap, Value) {
    request(
        a,
        "POST",
        path,
        body,
        cookie,
        csrf,
        Some("http://127.0.0.1:3000"),
    )
    .await
}
async fn signup(a: &Arc<App>, email: &str) -> (String, String) {
    let (s, h, v) = post(
        a,
        "/api/register",
        json!({"email":email,"password":"correct horse battery staple"}),
        "",
        "",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (
        h[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .into(),
        v["csrf"].as_str().unwrap().into(),
    )
}
fn credentials(email: &str, code: &str) -> Value {
    json!({"email":email,"password":"correct horse battery staple","code":code})
}

#[test]
fn youtube_url_allowlist_blocks_ssrf_and_injection() {
    for v in [
        "http://www.youtube.com/watch?v=abcdefghijk",
        "https://127.0.0.1/watch?v=abcdefghijk",
        "https://youtube.com.evil.org/watch?v=abcdefghijk",
        "https://youtube.com@evil.org/watch?v=abcdefghijk",
        "https://evil.org@youtube.com/watch?v=abcdefghijk",
        "file:///etc/passwd",
        "https://youtube.com:8443/watch?v=abcdefghijk",
        "https://youtu.be/../../etc/passwd",
        "https://youtube.com/redirect?q=http://127.0.0.1",
        "--exec=touch /tmp/injected",
        "https://youtube.com/playlist?list=abc",
        "https://youtu.be/abcdefghijk;touch /tmp/injected",
    ] {
        assert!(youtube_url(v).is_err(), "{v}");
    }
    for v in [
        "https://youtu.be/abcdefghijk?t=20",
        "https://m.youtube.com/watch?v=abcdefghijk&list=xyz",
        "https://www.youtube.com/shorts/abcdefghijk",
        "https://music.youtube.com/watch?v=abcdefghijk",
    ] {
        assert_eq!(
            youtube_url(v).unwrap(),
            "https://www.youtube.com/watch?v=abcdefghijk"
        );
    }
}
#[test]
fn totp_matches_rfc6238_and_rejects_replay() {
    let secret = b"12345678901234567890";
    for (t, expected) in [
        (59, "287082"),
        (1111111109, "081804"),
        (1111111111, "050471"),
        (1234567890, "005924"),
        (2000000000, "279037"),
        (20000000000, "353130"),
    ] {
        assert_eq!(totp(secret, t / 30), expected);
        assert_eq!(verify_totp(secret, expected, t, -1), Some(t / 30));
        assert!(verify_totp(secret, expected, t, t / 30).is_none());
    }
    assert!(verify_totp(secret, "12345x", 59, -1).is_none());
    assert!(verify_totp(secret, "1234567", 59, -1).is_none());
}
#[test]
fn encrypted_totp_detects_tampering() {
    let enc = seal(&[3; 32], b"a secret").unwrap();
    assert_eq!(open(&[3; 32], &enc).unwrap(), b"a secret");
    assert!(open(&[4; 32], &enc).is_err());
    let mut bytes = data_encoding::HEXLOWER.decode(enc.as_bytes()).unwrap();
    bytes[13] ^= 1;
    assert!(open(&[3; 32], &data_encoding::HEXLOWER.encode(&bytes)).is_err());
    assert!(open(&[3; 32], "00").is_err());
}

#[tokio::test]
async fn passwords_and_session_tokens_are_not_stored_in_plaintext() {
    let (_tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "alice@example.com").await;
    let (status, headers, v) = request(&a, "GET", "/api/me", json!({}), &cookie, "", None).await;
    assert_eq!(status, 200);
    assert_eq!(v["csrf"], csrf);
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    let db = a.db.lock().unwrap();
    let hash: String = db
        .query_row("SELECT password FROM users", [], |r| r.get(0))
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains("correct horse"));
    let token: String = db
        .query_row("SELECT token FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_ne!(cookie.split('=').nth(1).unwrap(), token);
}
#[tokio::test]
async fn cookie_security_and_production_mode() {
    let (_tmp, a) = fixture();
    assert!(
        a.cookie("test", 123)
            .contains("HttpOnly; SameSite=Strict; Max-Age=123")
    );
    assert!(!a.cookie("test", 123).contains("; Secure"));
    let tmp = TempDir::new().unwrap();
    let a = App::new(
        tmp.path().into(),
        [42; 32],
        "https://harbor.example".into(),
        "yt-dlp".into(),
    );
    assert_eq!(
        a.cookie("abc", 3),
        "__Host-harbor=abc; Path=/; HttpOnly; SameSite=Strict; Max-Age=3; Secure"
    );
}
#[tokio::test]
async fn csrf_and_origin_are_required() {
    let (_tmp, a) = fixture();
    let (c, k) = signup(&a, "a@example.com").await;
    for (csrf, origin) in [
        ("", Some("http://127.0.0.1:3000")),
        ("wrong", Some("http://127.0.0.1:3000")),
        (k.as_str(), Some("https://attacker.example")),
        (k.as_str(), None),
    ] {
        let (s, _, _) = request(&a, "POST", "/api/logout", json!({}), &c, csrf, origin).await;
        assert_eq!(s, 403);
    }
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &c, "", None)
            .await
            .0,
        200
    );
    assert_eq!(post(&a, "/api/logout", json!({}), &c, &k).await.0, 200);
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &c, "", None)
            .await
            .0,
        401
    );
}
#[tokio::test]
async fn session_survives_server_restart_but_expires() {
    let (tmp, a) = fixture();
    let (c, _) = signup(&a, "a@example.com").await;
    drop(a);
    let a = Arc::new(App::new(
        tmp.path().into(),
        [42; 32],
        "http://127.0.0.1:3000".into(),
        "yt-dlp".into(),
    ));
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &c, "", None)
            .await
            .0,
        200
    );
    a.db.lock()
        .unwrap()
        .execute("UPDATE sessions SET seen=?", [now() - 7 * 86400 - 1])
        .unwrap();
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &c, "", None)
            .await
            .0,
        200
    );
    a.db.lock()
        .unwrap()
        .execute(
            "UPDATE sessions SET seen=?,expires=?",
            params![now(), now() - 1],
        )
        .unwrap();
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &c, "", None)
            .await
            .0,
        401
    );
}
#[tokio::test]
async fn invalid_login_and_sql_injection_do_not_authenticate() {
    let (_tmp, a) = fixture();
    signup(&a, "a@example.com").await;
    for input in [
        json!({"email":"a@example.com","password":"wrong"}),
        json!({"email":"nobody@example.com","password":"correct horse battery staple"}),
        json!({"email":"' OR 1=1 --@example.com","password":"anything"}),
    ] {
        let (s, h, _) = post(&a, "/api/login", input, "", "").await;
        assert!(s.is_client_error());
        assert!(!h.contains_key(header::SET_COOKIE));
    }
    let (s, h, _) = post(&a, "/api/login", credentials("a@example.com", ""), "", "").await;
    assert_eq!(s, 200);
    assert!(h.contains_key(header::SET_COOKIE));
}
#[tokio::test]
async fn mfa_enrollment_login_replay_and_recovery() {
    let (_tmp, a) = fixture();
    let (c, k) = signup(&a, "a@example.com").await;
    let (_, other, _) = post(&a, "/api/login", credentials("a@example.com", ""), "", "").await;
    let other = other[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let (s, _, _) = post(&a, "/api/mfa/enroll", json!({"password":"wrong"}), &c, &k).await;
    assert_eq!(s, 401);
    let (s, _, v) = post(
        &a,
        "/api/mfa/enroll",
        json!({"password":"correct horse battery staple"}),
        &c,
        &k,
    )
    .await;
    assert_eq!(s, 200);
    assert!(
        v["qr"]
            .as_str()
            .unwrap()
            .starts_with("data:image/svg+xml;base64,")
    );
    let secret = data_encoding::BASE32_NOPAD
        .decode(v["secret"].as_str().unwrap().as_bytes())
        .unwrap();
    let (s, _, _) = post(&a, "/api/mfa/confirm", json!({"code":"xxxxxx"}), &c, &k).await;
    assert_eq!(s, 401);
    let (s, _, v) = post(
        &a,
        "/api/mfa/confirm",
        json!({"code":totp(&secret,now()/30)}),
        &c,
        &k,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["recovery_codes"].as_array().unwrap().len(), 8);
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &other, "", None)
            .await
            .0,
        401
    );
    let (_, h, p) = post(&a, "/api/login", credentials("a@example.com", ""), "", "").await;
    assert_eq!(p["mfa_required"], true);
    assert!(!h.contains_key(header::SET_COOKIE));
    // The enrollment code is consumed; a fresh step is required for login.
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("a@example.com", &totp(&secret, now() / 30)),
            "",
            ""
        )
        .await
        .0,
        401
    );
    let fresh = totp(&secret, now() / 30 + 1);
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("a@example.com", &fresh),
            "",
            ""
        )
        .await
        .0,
        200
    );
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("a@example.com", &fresh),
            "",
            ""
        )
        .await
        .0,
        401
    );
    let recovery = v["recovery_codes"][0].as_str().unwrap();
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("a@example.com", recovery),
            "",
            ""
        )
        .await
        .0,
        200
    );
    assert_eq!(
        post(
            &a,
            "/api/login",
            credentials("a@example.com", recovery),
            "",
            ""
        )
        .await
        .0,
        401
    );
    let db = a.db.lock().unwrap();
    let enc: String = db
        .query_row("SELECT totp FROM users", [], |r| r.get(0))
        .unwrap();
    assert_ne!(enc, v["secret"]);
    assert_eq!(
        db.query_row("SELECT count(*) FROM recovery", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
}
#[tokio::test]
async fn revoke_other_devices_requires_password() {
    let (_tmp, a) = fixture();
    let (c, k) = signup(&a, "a@example.com").await;
    let (_, h, _) = post(&a, "/api/login", credentials("a@example.com", ""), "", "").await;
    let other = h[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(
        post(
            &a,
            "/api/sessions/revoke",
            json!({"password":"wrong"}),
            &c,
            &k
        )
        .await
        .0,
        401
    );
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), other, "", None)
            .await
            .0,
        200
    );
    assert_eq!(
        post(
            &a,
            "/api/sessions/revoke",
            json!({"password":"correct horse battery staple"}),
            &c,
            &k
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), other, "", None)
            .await
            .0,
        401
    );
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &c, "", None)
            .await
            .0,
        200
    );
}
#[tokio::test]
async fn download_files_and_history_are_private() {
    let (_tmp, a) = fixture();
    let (alice, _) = signup(&a, "a@example.com").await;
    let (bob, _) = signup(&a, "b@example.com").await;
    let id = uuid::Uuid::new_v4().to_string();
    let dir = a.data.join("downloads").join(&id);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("media.mp4"), b"test media").unwrap();
    a.db.lock().unwrap().execute("INSERT INTO jobs VALUES(?,1,'Private title','video','1080','ready',NULL,?,'media.mp4')",params![id,now()]).unwrap();
    a.progress.lock().unwrap().insert(
        id.clone(),
        media::JobProgress {
            phase: "video",
            percent: Some(42.0),
        },
    );
    let (_, _, v) = request(&a, "GET", "/api/jobs", json!({}), &bob, "", None).await;
    assert!(v["jobs"].as_array().unwrap().is_empty());
    let path = format!("/api/jobs/{id}/file");
    assert_eq!(
        request(&a, "GET", &path, json!({}), &bob, "", None).await.0,
        404
    );
    assert_eq!(
        request(&a, "GET", &path, json!({}), "", "", None).await.0,
        401
    );
    let (status, headers, _) = request(&a, "GET", &path, json!({}), &alice, "", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"media.mp4\""
    );
    a.db.lock()
        .unwrap()
        .execute("UPDATE jobs SET created=?", [now() - 86401])
        .unwrap();
    assert_eq!(
        request(&a, "GET", &path, json!({}), &alice, "", None)
            .await
            .0,
        404
    );
    media::cleanup(&a).await;
    assert!(!dir.exists());
}
#[tokio::test]
async fn malformed_and_uninspected_jobs_are_rejected() {
    let (_tmp, a) = fixture();
    let (c, k) = signup(&a, "a@example.com").await;
    assert_eq!(
        post(
            &a,
            "/api/jobs",
            json!({"url":"https://youtu.be/abcdefghijk","kind":"video","quality":"1080; calc"}),
            &c,
            &k
        )
        .await
        .0,
        400
    );
    assert_eq!(
        post(
            &a,
            "/api/inspect",
            json!({"url":"http://localhost:22"}),
            &c,
            &k
        )
        .await
        .0,
        400
    );
    assert_eq!(
        post(
            &a,
            "/api/inspect",
            json!({"url":"https://youtu.be/abcdefghijk"}),
            &c,
            &k
        )
        .await
        .0,
        400
    );
    let (_, _, v) = request(&a, "GET", "/api/jobs", json!({}), &c, "", None).await;
    assert!(v["jobs"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn oversized_payload_and_auth_rate_limit() {
    let (_tmp, a) = fixture();
    let (s, _, _) = post(
        &a,
        "/api/register",
        json!({"email":"a@example.com","password":"x".repeat(9000)}),
        "",
        "",
    )
    .await;
    assert_eq!(s, 413);
    for _ in 0..19 {
        post(
            &a,
            "/api/login",
            json!({"email":"invalid","password":"x"}),
            "",
            "",
        )
        .await;
    }
    let (s, _, _) = post(
        &a,
        "/api/login",
        json!({"email":"invalid","password":"x"}),
        "",
        "",
    )
    .await;
    assert_eq!(s, 429);
}

#[tokio::test]
async fn session_policy_is_authorized_persistent_and_applies_to_new_logins() {
    let (tmp, a) = fixture();
    let (cookie, csrf) = signup(&a, "session@example.test").await;
    let route = "/api/admin/session-policy";
    let policy = json!({"hours":48});
    assert_eq!(
        request(&a, "GET", route, json!({}), "", "", None).await.0,
        401
    );
    assert_eq!(
        request(&a, "GET", route, json!({}), &cookie, "", None)
            .await
            .0,
        403
    );
    assert_eq!(post(&a, route, policy.clone(), &cookie, &csrf).await.0, 403);
    admin::set_role(&a.db.lock().unwrap(), "grant", "session@example.test").unwrap();
    assert_eq!(
        request(&a, "GET", route, json!({}), &cookie, "", None)
            .await
            .2["hours"],
        720
    );
    assert_eq!(post(&a, route, policy.clone(), &cookie, "").await.0, 403);
    for hours in [0, 8761] {
        assert_eq!(
            post(&a, route, json!({"hours":hours}), &cookie, &csrf)
                .await
                .0,
            400
        );
    }
    let old_expiry: i64 =
        a.db.lock()
            .unwrap()
            .query_row("SELECT expires FROM sessions", [], |r| r.get(0))
            .unwrap();
    assert_eq!(
        post(&a, route, policy.clone(), &cookie, &csrf).await.2,
        policy
    );
    let db = Connection::open(tmp.path().join("harbor.sqlite3")).unwrap();
    admin::initialize(&db).unwrap();
    assert_eq!(admin::session_policy(&db).unwrap().hours, 48);
    assert_eq!(
        db.query_row("SELECT expires FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        old_expiry
    );
    // Idle sessions survive both periodic cleanup and new session issuance.
    db.execute("UPDATE sessions SET seen=?", [now() - 8 * 86400])
        .unwrap();
    media::cleanup(&a).await;
    let before = now();
    let (status, headers, _) = post(
        &a,
        "/api/login",
        json!({"email":"session@example.test","password":"correct horse battery staple"}),
        "",
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=172800")
    );
    let new_expiry: i64 = db
        .query_row("SELECT MIN(expires) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert!((before + 48 * 3600..=now() + 48 * 3600).contains(&new_expiry));
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &cookie, "", None)
            .await
            .0,
        200
    );
    assert_eq!(
        db.query_row("SELECT MAX(expires) FROM sessions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        old_expiry
    );
    db.execute("UPDATE sessions SET expires=?", [now()])
        .unwrap();
    assert_eq!(
        request(&a, "GET", "/api/me", json!({}), &cookie, "", None)
            .await
            .0,
        401
    );
}
