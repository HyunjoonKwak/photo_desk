use super::*;
use crate::db::conn::Db;

/// 구버전 DB를 만든다 — 라이브러리 층이 생기기 전 모양.
fn legacy_db(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("old.db");
    let c = Connection::open(&path).unwrap();
    c.execute_batch(include_str!("../schema.sql")).unwrap();
    c.execute_batch(
        "DROP INDEX IF EXISTS idx_folders_lib;
             ALTER TABLE folders DROP COLUMN library_id;
             DROP TABLE libraries;",
    )
    .unwrap();
    c.execute_batch(
        "INSERT INTO volumes(uuid,name,role) VALUES('V','MAIN SSD','library');
             INSERT INTO folders(volume_uuid,rel_path,name,area) VALUES
               ('V','MERGE/사진/연도별/2003','2003',1),
               ('V','MERGE/사진/연도별/2004','2004',1),
               ('V','MERGE/사진/주제별/여행','여행',1);",
    )
    .unwrap();
    path
}

#[test]
fn trash_columns_are_added_to_an_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "DROP INDEX IF EXISTS idx_files_trashed;
                 DROP INDEX IF EXISTS idx_files_folder_live;
                 ALTER TABLE files DROP COLUMN trashed_at;
                 ALTER TABLE files DROP COLUMN trash_path;
                 ALTER TABLE files DROP COLUMN trash_batch;",
        )
        .unwrap();
        assert!(!has_column(&c, "files", "trashed_at").unwrap());
    }
    let db = Db::open(&path).expect("구버전 DB도 열려야 한다");
    db.read(|c| {
            assert!(has_column(c, "files", "trashed_at")?);
            assert!(has_column(c, "files", "trash_path")?);
            assert!(has_column(c, "files", "trash_batch")?);
            // 라이브러리 장수를 세는 부분 인덱스도 되살아나야 한다 — 없으면 첫 화면이
            // 다시 3초로 돌아간다 (실측 2026-09-01)
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_files_folder_live'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(n, 1, "구버전 DB 를 올린 뒤 idx_files_folder_live 가 없다");
            Ok(())
        })
        .unwrap();
}

#[test]
fn release_091_integrity_upgrade_is_idempotent_on_a_090_database() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(include_str!("../schema.sql")).unwrap();
    c.execute_batch(
        "ALTER TABLE capture_date_journal DROP COLUMN before_sha256;
             ALTER TABLE capture_date_journal DROP COLUMN after_sha256;
             ALTER TABLE capture_date_journal DROP COLUMN undone_at;
             DROP TABLE copy_manifest;",
    )
    .unwrap();

    add_release_091_integrity(&c).unwrap();
    add_release_091_integrity(&c).unwrap();
    for column in ["before_sha256", "after_sha256", "undone_at"] {
        assert!(has_column(&c, "capture_date_journal", column).unwrap());
    }
    let table: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='copy_manifest'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table, 1);
    let index: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_publication_batch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(index, 1);
}

/// 이 순서를 틀리면 앱이 아예 뜨지 않는다. `schema.sql`이 먼저 도는데
/// 구버전 DB에는 그 시점에 `library_id`가 없어 배치 전체가 실패했다.
#[test]
fn opens_a_database_that_predates_libraries() {
    let dir = tempfile::tempdir().unwrap();
    let path = legacy_db(dir.path());

    let db = Db::open(&path).expect("구버전 DB도 열려야 한다");

    let (id, rel, name): (i64, String, String) = db
        .read(|c| {
            c.query_row("SELECT id, rel_path, name FROM libraries", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
        })
        .expect("라이브러리 하나가 만들어져야 한다");
    assert_eq!(rel, "MERGE/사진", "공통 앞부분이 그 시절의 루트다");
    assert_eq!(name, "사진");

    let attached: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM folders WHERE library_id = ?1",
                [id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(attached, 3, "폴더가 전부 붙어야 한다");
}

#[test]
fn image_hash_column_is_added_to_an_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "DROP INDEX IF EXISTS idx_files_image_hash;
                 ALTER TABLE files DROP COLUMN image_hash;",
        )
        .unwrap();
        assert!(!has_column(&c, "files", "image_hash").unwrap());
    }
    let db = Db::open(&path).expect("그림 해시 전 DB도 열려야 한다");
    db.read(|c| {
        assert!(has_column(c, "files", "image_hash")?);
        Ok(())
    })
    .unwrap();
    db.write(|c| c.execute("UPDATE files SET image_hash='abc'", []))
        .unwrap();
}

#[test]
fn done_at_column_is_added_to_an_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch("ALTER TABLE groups DROP COLUMN done_at;")
            .unwrap();
    }
    let db = Db::open(&path).expect("done_at 전 DB도 열려야 한다");
    db.write(|c| c.execute("UPDATE groups SET done_at = 1", []))
        .unwrap();
}

/// 업그레이드가 나중에 더하는 칸을 schema.sql 이 먼저 참조하면 구버전 DB 가 안 열린다.
/// v0.5.4 DB 에서 실제로 «no such column: geo_country» 로 죽었다 (2026-09-01).
/// 앱을 열 때마다 도는 보정이 «다시 물어볼 자리»를 «없는 자리»로 굳히면 안 된다.
///
/// 오프라인 판정이 못 정한 자리는 이름이 비어 있는 것이 정상이다. 그것을
/// 지우면 온라인 보강 대상에서 영영 빠진다 (2026-09-01 외부 검토).
#[test]
fn the_repair_never_settles_a_cell_that_is_still_waiting_for_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = Db::open(&path).unwrap();
        db.write(|c| {
            c.execute_batch(
                "INSERT INTO places(cell,status,source,precision,at)
                       VALUES('1,1','unresolved','offline_geonames','approximate',0),
                             ('2,2','ok','offline_geonames','approximate',0),
                             ('3,3','ok','nominatim','remote',0);
                     UPDATE places SET country='대한민국', name='대한민국'
                      WHERE cell IN ('2,2','3,3');
                     -- 값이 비었는데 성공이라고 적힌 모순 행 — 이것만 고쳐야 한다
                     INSERT INTO places(cell,status,source,precision,at)
                       VALUES('4,4','ok','nominatim','remote',0);",
            )
        })
        .unwrap();
    }
    // 다시 열어 보정을 한 번 더 돌린다 (실제로 앱을 껐다 켜는 것과 같다)
    let db = Db::open(&path).unwrap();
    let status = |cell: &str| -> String {
        db.read(|c| {
            c.query_row("SELECT status FROM places WHERE cell=?1", [cell], |r| {
                r.get(0)
            })
        })
        .unwrap()
    };
    assert_eq!(
        status("1,1"),
        "unresolved",
        "아직 물어볼 자리를 굳히면 안 된다"
    );
    assert_eq!(status("2,2"), "ok");
    assert_eq!(status("3,3"), "ok");
    assert_eq!(
        status("4,4"),
        "none",
        "값이 비었는데 성공이라고 적힌 행은 고친다"
    );
}

/// 갓 만든 DB 와 옛 DB 를 올린 것이 **같은 모양**이어야 한다.
///
/// 칸이나 인덱스를 한쪽에만 더하면 조용히 갈라진다 — 새로 설치한 사람만
/// 인덱스가 없어 느리거나, 옛 사용자만 칸이 없어 질의가 깨진다.
///
/// 만들어진 SQL 글월을 그대로 견주지는 않는다. `ALTER TABLE ADD COLUMN` 은
/// 칸을 늘 끝에 붙이므로 순서와 주석이 달라진다 — 그것은 차이가 아니다.
/// 이름의 집합만 본다.
#[test]
fn a_fresh_database_and_an_upgraded_one_end_up_identical() {
    let dir = tempfile::tempdir().unwrap();

    /// 표·인덱스 이름과 각 표의 칸 이름 — 순서에 흔들리지 않게 모두 정렬한다
    fn shape(db: &Db) -> Vec<String> {
        db.read(|c| {
                let mut names: Vec<(String, String)> = {
                    let mut st = c.prepare(
                        "SELECT type, name FROM sqlite_master
                          WHERE name NOT LIKE 'sqlite_%' AND type IN ('table','index','view','trigger')",
                    )?;
                    let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                    it.collect::<rusqlite::Result<Vec<_>>>()?
                };
                names.sort();
                let mut out = Vec::new();
                for (kind, name) in names {
                    if kind == "table" {
                        let mut st = c.prepare(&format!("PRAGMA table_info({name})"))?;
                        let it = st.query_map([], |r| r.get::<_, String>(1))?;
                        let mut cols = it.collect::<rusqlite::Result<Vec<_>>>()?;
                        cols.sort();
                        out.push(format!("table {name}({})", cols.join(",")));
                    } else {
                        out.push(format!("{kind} {name}"));
                    }
                }
                Ok(out)
            })
            .unwrap()
    }

    let fresh = Db::open(dir.path().join("fresh.db")).unwrap();
    let want = shape(&fresh);

    // 0.5.4 판의 모양으로 되돌린 DB 를 올린다. geo_name 은 v2 첫 스키마부터
    // 있었으므로 남긴다 — 실제로 존재했던 판을 흉내 내야 뜻이 있다.
    let old_path = dir.path().join("old.db");
    {
        let c = Connection::open(&old_path).unwrap();
        let create: Vec<String> = fresh
                .read(|f| {
                    let mut st = f.prepare(
                        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",
                    )?;
                    let it = st.query_map([], |r| r.get::<_, String>(0))?;
                    it.collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap();
        for sql in &create {
            c.execute_batch(&format!("{sql};")).unwrap();
        }
        c.execute_batch(
            "DROP INDEX IF EXISTS idx_files_geo;
                 DROP INDEX IF EXISTS idx_places_status;
                 DROP TABLE IF EXISTS places;
                 ALTER TABLE files DROP COLUMN geo_country;
                 ALTER TABLE files DROP COLUMN geo_admin1;
                 ALTER TABLE files DROP COLUMN geo_admin2;",
        )
        .unwrap();
    }
    let got = shape(&Db::open(&old_path).unwrap());

    let missing: Vec<_> = want.iter().filter(|x| !got.contains(x)).collect();
    let extra: Vec<_> = got.iter().filter(|x| !want.contains(x)).collect();
    assert!(
            missing.is_empty() && extra.is_empty(),
            "갓 만든 DB 와 올린 DB 의 모양이 다릅니다\n올린 쪽에 없는 것: {missing:#?}\n올린 쪽에만 있는 것: {extra:#?}"
        );

    // 이 시험이 실제로 무언가를 지키는지 — 지명 인덱스가 양쪽에 다 있어야 한다
    assert!(
        want.contains(&"index idx_files_geo".to_string()),
        "새 DB 에 지명 인덱스가 없습니다"
    );
    assert!(
        want.iter().any(|x| x.starts_with("table places(")),
        "새 DB 에 places 표가 없습니다"
    );
}

/// 새 칸을 넣을 때마다 이 목록에 더한다 — 사람이 기억하지 않아도 시험이 잡게.
#[test]
fn schema_never_mentions_a_column_that_upgrade_adds_later() {
    let schema = include_str!("../schema.sql");
    for col in [
        "trashed_at",
        "trash_path",
        "trash_batch",
        "faces_at",
        "image_hash",
        "done_at",
        "geo_country",
        "geo_admin1",
        "geo_admin2",
    ] {
        for line in schema.lines() {
            let l = line.trim();
            if l.starts_with("CREATE INDEX") && l.contains(col) {
                panic!("schema.sql 의 인덱스가 upgrade 전용 칸 «{col}»을 참조한다 — 구버전 DB 가 안 열린다:\n{l}");
            }
        }
    }
}

/// 지명 칸이 없던 DB(v0.5.4)도 그대로 열려야 한다
#[test]
fn a_database_from_before_place_names_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "DROP INDEX IF EXISTS idx_files_geo;
                 ALTER TABLE files DROP COLUMN geo_country;
                 ALTER TABLE files DROP COLUMN geo_admin1;
                 ALTER TABLE files DROP COLUMN geo_admin2;
                 DROP TABLE IF EXISTS places;",
        )
        .unwrap();
        assert!(!has_column(&c, "files", "geo_country").unwrap());
    }
    let db = Db::open(&path).expect("지명 칸이 없던 DB 도 열려야 한다");
    db.read(|c| {
        assert!(has_column(c, "files", "geo_country")?);
        assert!(has_column(c, "files", "geo_admin2")?);
        Ok(())
    })
    .unwrap();
    db.write(|c| c.execute("INSERT INTO places(cell,at) VALUES('0.00,0.00',0)", []))
        .unwrap();
}

/// 첫 지명 빌드가 남긴 빈 캐시는 status 칸이 없었다. 새 칸의 기본값 'ok'를
/// 그대로 주면 성공으로 오인하므로, 이름 없는 행은 'none'으로 복구해야 한다.
#[test]
fn empty_place_rows_from_the_first_geo_build_become_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "DROP TABLE places;
                 CREATE TABLE places (
                   cell TEXT PRIMARY KEY, country TEXT, admin1 TEXT, admin2 TEXT,
                   name TEXT, at INTEGER NOT NULL
                 );
                 INSERT INTO places(cell,country,admin1,admin2,name,at)
                   VALUES('10.00,20.00',NULL,NULL,NULL,NULL,0),
                         ('37.28,127.05','대한민국','경기도','수원시','수원시',0);",
        )
        .unwrap();
    }

    let db = Db::open(&path).expect("첫 지명 빌드의 DB도 열려야 한다");
    let statuses: Vec<(String, String)> = db
        .read(|c| {
            let mut st = c.prepare("SELECT cell,status FROM places ORDER BY cell")?;
            let rows = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(
        statuses,
        vec![
            ("10.00,20.00".into(), "none".into()),
            ("37.28,127.05".into(), "ok".into())
        ]
    );

    // 중간 수정 빌드가 이미 status='ok'를 붙인 DB도 다음 실행에서 복구한다.
    db.write(|c| c.execute("UPDATE places SET status='ok' WHERE cell='10.00,20.00'", []))
        .unwrap();
    drop(db);
    let reopened = Db::open(&path).unwrap();
    let repaired: String = reopened
        .read(|c| {
            c.query_row(
                "SELECT status FROM places WHERE cell='10.00,20.00'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(repaired, "none");
}

/// 출처·정밀도 칸이 없던 DB 도 열리고, 기존 성공 캐시는 온라인 결과로 표시된다
#[test]
fn place_metadata_columns_are_added_and_existing_cache_is_labelled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "DROP TABLE places;
                 CREATE TABLE places (
                   cell TEXT PRIMARY KEY, country TEXT, admin1 TEXT, admin2 TEXT, name TEXT,
                   status TEXT NOT NULL DEFAULT 'ok', at INTEGER NOT NULL
                 );
                 INSERT INTO places(cell,country,admin1,admin2,name,status,at)
                   VALUES('37.28,127.05','대한민국','경기도','수원시','수원시','ok',111),
                         ('10.00,20.00',NULL,NULL,NULL,NULL,'none',222);",
        )
        .unwrap();
    }
    let db = Db::open(&path).expect("출처 칸이 없던 DB 도 열려야 한다");
    let rows: Vec<(String, String, String, Option<String>, i64)> = db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT cell, status, source, precision, resolved_at FROM places ORDER BY cell",
            )?;
            let out = st
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            out
        })
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "10.00,20.00".into(),
                "none".into(),
                "nominatim".into(),
                None,
                222
            ),
            (
                "37.28,127.05".into(),
                "ok".into(),
                "nominatim".into(),
                Some("remote".into()),
                111
            ),
        ],
        "기존 캐시는 값이 그대로이고 출처만 붙는다"
    );
    // 두 번 열어도 그대로 (멱등)
    drop(db);
    let again = Db::open(&path).unwrap();
    let n: i64 = again
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM places WHERE source='nominatim'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(n, 2);
}

#[test]
fn upgrading_twice_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = legacy_db(dir.path());
    let count = |db: &Db| -> i64 {
        db.read(|c| c.query_row("SELECT COUNT(*) FROM libraries", [], |r| r.get(0)))
            .unwrap()
    };
    assert_eq!(count(&Db::open(&path).unwrap()), 1);
    assert_eq!(count(&Db::open(&path).unwrap()), 1, "두 번 열어도 하나");
}

#[test]
fn old_floating_photo_dates_are_migrated_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    let old = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .unwrap()
        .and_hms_opt(18, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp();
    db.write(|c| {
        c.execute_batch("DELETE FROM settings WHERE key='internal.taken_at_utc_v1';")?;
        c.execute(
            "INSERT INTO volumes(uuid,name,role) VALUES('V','v','library')",
            [],
        )?;
        c.execute(
            "INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','p','p',1)",
            [],
        )?;
        c.execute(
            "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at)
                 VALUES(1,1,'photo.jpg',1,0,?1,0,0)",
            [old],
        )?;
        migrate_taken_at_to_utc(c)
    })
    .unwrap();

    let migrated: i64 = db
        .read(|c| c.query_row("SELECT taken_at FROM files", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(
        migrated,
        crate::media::taken_at::civil_to_unix(2024, 1, 1, 18, 0, 0)
    );
    db.write(migrate_taken_at_to_utc).unwrap();
    let again: i64 = db
        .read(|c| c.query_row("SELECT taken_at FROM files", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(
        again, migrated,
        "두 번 열어도 다시 시간대를 적용하면 안 된다"
    );
}

#[test]
fn every_taken_at_source_keeps_the_previous_migration_result() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    let old = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .unwrap()
        .and_hms_opt(18, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp();
    db.write(|c| {
        c.execute_batch(
            "DELETE FROM settings WHERE key='internal.taken_at_utc_v1';
                 INSERT INTO volumes(uuid,name,role) VALUES('V','v','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area)
                   VALUES(1,'V','album/20230304_050607','dates',1);",
        )?;
        for (id, name, kind, source) in [
            (1, "photo.jpg", 0, 0),
            (2, "video.mov", 1, 0),
            (3, "20240203_040506.jpg", 0, 1),
            (4, "folder-date.jpg", 0, 1),
            (5, "mtime.jpg", 0, 2),
            (6, "unknown.jpg", 0, 3),
            (7, "override.jpg", 0, 4),
        ] {
            c.execute(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at)
                     VALUES(?1,1,?2,1,?3,?4,?5,0)",
                rusqlite::params![id, name, kind, old, source],
            )?;
        }
        migrate_taken_at_to_utc_in_chunks(c, 1)
    })
    .unwrap();

    let got: Vec<(i64, i64)> = db
        .read(|c| {
            let mut st = c.prepare("SELECT id,taken_at FROM files ORDER BY id")?;
            let mapped = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap();
    assert_eq!(
        got,
        vec![
            (1, crate::media::taken_at::floating_civil_to_unix(old)),
            (2, old),
            (
                3,
                crate::media::taken_at::from_filename("20240203_040506.jpg").unwrap()
            ),
            (
                4,
                crate::media::taken_at::from_filename("20230304_050607").unwrap()
            ),
            (5, old),
            (6, old),
            (7, old),
        ]
    );
}

#[test]
fn prefix_is_cut_at_slashes() {
    // 글자 단위였다면 "…/연도별/200"이 나온다
    let p = vec![
        "MERGE/사진통합작업/연도별/2003".to_string(),
        "MERGE/사진통합작업/연도별/2004".to_string(),
    ];
    assert_eq!(common_dir_prefix(&p), "MERGE/사진통합작업/연도별");
}

#[test]
fn diverging_subtrees_stop_at_their_parent() {
    // 실제 데이터 모양: 한 루트 아래 연도별/주제별로 갈린다
    let p = vec![
        "MERGE/사진통합작업/연도별/2001".to_string(),
        "MERGE/사진통합작업/주제별/참고이미지들".to_string(),
    ];
    assert_eq!(common_dir_prefix(&p), "MERGE/사진통합작업");
}

#[test]
fn no_common_prefix_means_volume_root() {
    // PHOTO 1처럼 볼륨 최상단을 통째로 잡은 경우
    let p = vec!["가족사진/2003".to_string(), "황금부엉이/Book1".to_string()];
    assert_eq!(common_dir_prefix(&p), "");
    let p = vec!["2003".to_string(), "2004".to_string()];
    assert_eq!(common_dir_prefix(&p), "");
}

#[test]
fn a_parent_in_the_list_is_the_answer() {
    // 루트에 사진이 흩어져 있으면 루트도 목록에 들어 있다
    let p = vec![
        "MERGE/사진통합작업".to_string(),
        "MERGE/사진통합작업/연도별".to_string(),
    ];
    assert_eq!(common_dir_prefix(&p), "MERGE/사진통합작업");
}

#[test]
fn empty_input() {
    assert_eq!(common_dir_prefix(&[]), "");
}
