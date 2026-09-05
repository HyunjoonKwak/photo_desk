use super::*;

/// 영상 메타데이터를 아직 안 읽었으면 다시 스캔 대상이 되어야 한다.
/// 안 그러면 이미 들어 있는 2,828개는 영영 길이도 촬영일도 못 얻는다.
#[test]
fn videos_without_metadata_are_rescanned_once() {
    use super::*;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.jpg"), b"x".repeat(50)).unwrap();
    std::fs::write(dir.path().join("v.mp4"), b"y".repeat(50)).unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();

    let lib: i64 = db
        .read(|c| c.query_row("SELECT id FROM libraries", [], |r| r.get(0)))
        .unwrap();
    // 스캔이 끝나면 영상은 0(=읽어 봤지만 없음)이 찍혀 다시 대상이 되지 않는다
    let known = load_known(&db, lib).unwrap();
    assert_eq!(known.len(), 2, "둘 다 아는 파일이어야 한다");

    // 구버전에서 넘어온 것처럼 NULL로 되돌려 본다
    db.write(|c| c.execute("UPDATE files SET duration_ms=NULL WHERE kind=1", []))
        .unwrap();
    let known = load_known(&db, lib).unwrap();
    assert_eq!(known.len(), 1, "영상은 다시 읽을 대상이 된다");
}

/// 스캔이 끝나면 이미 아는 지명이 새 사진에 붙어 있어야 한다.
///
/// 전체 스캔·가져오기·폴더 감시·EXIF 재읽기가 모두 이 함수를 지나므로,
/// 여기서 붙으면 네 경로 모두에서 붙는다. 그리고 이 일은 `Ok` 를 돌려주기
/// **전**에 끝나므로, `scan-done` 을 받은 화면은 이미 이름을 본다.
#[test]
fn a_scan_finishes_with_known_place_names_already_applied() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.jpg"), b"x".repeat(50)).unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();

    // 그 사진에 좌표가 있고, 그 자리의 이름을 이미 안다고 하자
    db.write(|c| {
            c.execute_batch(
                "UPDATE files SET gps_lat=37.2911, gps_lon=127.0089 WHERE name='a.jpg';
                 INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,at)
                   VALUES('37.29,127.00','대한민국','경기도','수원시','수원시','ok','offline_geonames','approximate',0);",
            )
        })
        .unwrap();
    let named = |n: &str| -> Option<String> {
        db.read(|c| {
            c.query_row("SELECT geo_name FROM files WHERE name=?1", [n], |r| {
                r.get(0)
            })
        })
        .unwrap()
    };
    assert_eq!(named("a.jpg"), None, "아직 붙지 않았다");

    // 사진이 하나 더 들어와 스캔이 다시 돈다 (가져오기·감시도 같은 길이다)
    std::fs::write(dir.path().join("b.jpg"), b"y".repeat(50)).unwrap();
    let lib: i64 = db
        .read(|c| c.query_row("SELECT id FROM libraries", [], |r| r.get(0)))
        .unwrap();
    let p = scan_folder(&db, lib, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(p.inserted, 1);

    // 스캔이 돌아온 시점에 이미 붙어 있다 — 사용자가 아무것도 누르지 않았다
    assert_eq!(named("a.jpg").as_deref(), Some("수원시"));
}

/// 바뀐 것이 없는 스캔은 전파를 건너뛴다 — 감시는 폴더마다 이 함수를 부른다
#[test]
fn a_scan_that_changed_nothing_does_not_touch_the_photos() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.jpg"), b"x".repeat(50)).unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    db.write(|c| {
            c.execute_batch(
                "UPDATE files SET gps_lat=37.2911, gps_lon=127.0089 WHERE name='a.jpg';
                 INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,at)
                   VALUES('37.29,127.00','대한민국','경기도','수원시','수원시','ok','offline_geonames','approximate',0);",
            )
        })
        .unwrap();

    let lib: i64 = db
        .read(|c| c.query_row("SELECT id FROM libraries", [], |r| r.get(0)))
        .unwrap();
    let p = scan_folder(&db, lib, dir.path(), 0, |_| {}).unwrap();
    assert_eq!((p.inserted, p.updated), (0, 0), "바뀐 것이 없다");
    let name: Option<String> = db
        .read(|c| {
            c.query_row("SELECT geo_name FROM files WHERE name='a.jpg'", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(name, None, "할 일이 없으면 파일 표를 훑지도 않는다");
}

#[test]
fn nfc_normalizes_hangul() {
    // macOS가 주는 NFD 표기 (자모 분리)
    let nfd = "\u{1112}\u{1161}\u{11AB}"; // 한
    let nfc_str = "\u{D55C}"; // 한
    assert_ne!(nfd, nfc_str, "원래는 다른 문자열이다");
    assert_eq!(nfc(nfd), nfc_str, "NFC로 맞춰져야 한다");
    assert_eq!(nfc(nfc_str), nfc_str, "이미 NFC면 그대로");
}

#[test]
fn scans_a_directory_and_stores_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("2026").join("2026-08-25 테스트");
    std::fs::create_dir_all(&sub).unwrap();
    // 실제 JPEG이 아니어도 경로·크기는 기록된다
    std::fs::write(sub.join("20260825_143000.jpg"), b"x".repeat(100)).unwrap();
    std::fs::write(sub.join("readme.txt"), b"ignored").unwrap();

    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    let p = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(p.inserted, 1, "미디어 파일만 들어가야 한다");

    // 저장된 경로가 절대경로가 아니어야 한다
    let rel: String = db
        .read(|c| {
            c.query_row(
                "SELECT fo.rel_path FROM files fi JOIN folders fo ON fo.id=fi.folder_id",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(!rel.starts_with('/'), "상대경로여야 한다: {rel}");
    assert!(rel.contains("2026-08-25 테스트"), "실제 경로: {rel}");
}

#[test]
fn taken_at_comes_from_the_filename_when_there_is_no_exif() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("20200505_101112.jpg"), b"x").unwrap();
    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();

    let (ts, src): (i64, i32) = db
        .read(|c| {
            c.query_row("SELECT taken_at, taken_at_source FROM files", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })
        .unwrap();
    assert_eq!(src, taken_at::Source::Filename as i32);
    assert_eq!(ts, taken_at::civil_to_unix(2020, 5, 5, 10, 11, 12));
}

#[test]
fn rescanning_skips_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("20260101_120000.jpg"), b"hello").unwrap();
    let db = Db::open(dir.path().join("db.sqlite")).unwrap();

    let first = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(first.inserted, 1);
    assert_eq!(first.skipped, 0);

    let second = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(second.skipped, 1, "바뀌지 않았으면 건너뛴다");
    assert_eq!(second.inserted, 0);
}

#[test]
fn changed_file_is_rescanned() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("20260101_120000.jpg");
    std::fs::write(&f, b"hello").unwrap();
    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();

    // 크기를 바꾸면 다시 읽어야 한다
    std::fs::write(&f, b"hello world, longer now").unwrap();
    let again = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(again.skipped, 0);
    assert!(again.inserted + again.updated >= 1);
}

#[test]
fn changed_file_replaces_source_metadata_and_invalidates_derived_values() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("20260101_120000.jpg");
    std::fs::write(&f, b"first body").unwrap();
    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    db.write(|c| {
        c.execute(
            "UPDATE files SET cam_model='old camera', orientation=6,
                gps_lat=37.5, gps_lon=127.0, geo_name='old place',
                sharpness=1.0, exposure=2.0, embedding=X'01', faces_at=123,
                phash=456, psig=X'0102'",
            [],
        )
    })
    .unwrap();

    std::fs::write(&f, b"second body is longer").unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    let stale: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE cam_model IS NOT NULL OR orientation IS NOT NULL
                OR gps_lat IS NOT NULL OR geo_name IS NOT NULL OR sharpness IS NOT NULL
                OR exposure IS NOT NULL OR embedding IS NOT NULL OR faces_at IS NOT NULL
                OR phash IS NOT NULL OR psig IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(stale, 0);
}

#[test]
fn system_folders_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    for d in ["@eaDir", ".Spotlight-V100", "#recycle"] {
        let p = dir.path().join(d);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("20260101_120000.jpg"), b"x").unwrap();
    }
    std::fs::write(dir.path().join("20260102_120000.jpg"), b"x").unwrap();

    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    let p = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(p.inserted, 1, "시스템 폴더는 건너뛴다");
}

/// 파인더에서 지운 것은 스캔이 못 본다 — prune이 뺀다. 휴지통 것은 둔다.
/// Finder 에서 폴더째 지운 뒤 다시 스캔하면 그 파일·폴더 행이 사라진다
#[test]
fn a_full_rescan_drops_rows_for_folders_deleted_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    for d in ["2003/2004", "2005"] {
        let p = dir.path().join(d);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("a.jpg"), b"photo ".repeat(20)).unwrap();
        std::fs::write(p.join("b.jpg"), b"other ".repeat(20)).unwrap();
    }
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    let count = |sql: &str| -> i64 { db.read(|c| c.query_row(sql, [], |r| r.get(0))).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM files"), 4);
    std::fs::remove_dir_all(dir.path().join("2003")).unwrap();
    let p = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!((p.removed, p.folders_removed), (2, 1), "{p:?}");
    assert_eq!(count("SELECT COUNT(*) FROM files"), 2);
    assert_eq!(
        count("SELECT COUNT(*) FROM folders WHERE rel_path LIKE '%2004'"),
        0,
        "빈 폴더 행도"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM folders WHERE rel_path LIKE '%2005'"),
        1
    );
}

/// 마운트가 빠져 아무것도 안 보이면 지우지 않는다
#[test]
fn a_rescan_that_sees_nothing_does_not_prune() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("a.jpg"), b"photo ".repeat(20)).unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    std::fs::remove_dir_all(&p).unwrap();
    let r = scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    assert_eq!(
        r.removed, 0,
        "훑은 것이 0이면 마운트 문제로 보고 손대지 않는다"
    );
    let n: i64 = db
        .read(|c| c.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn prune_removes_rows_whose_files_are_gone() {
    let dir = tempfile::tempdir().unwrap();
    let lib_dir = dir.path().join("lib");
    let sub = lib_dir.join("2024");
    std::fs::create_dir_all(&sub).unwrap();
    for n in ["a.jpg", "b.jpg", "c.jpg"] {
        std::fs::write(sub.join(n), b"x").unwrap();
    }
    let db = Db::open(dir.path().join("t.db")).unwrap();
    let p = scan_test(&db, &lib_dir, 1, |_| {}).unwrap();
    assert_eq!(p.inserted, 3);
    let lib: i64 = db
        .read(|c| c.query_row("SELECT id FROM libraries", [], |r| r.get(0)))
        .unwrap();
    let mount = crate::db::volumes::describe(&lib_dir).unwrap().mount_path;
    let rel_dir: String = db
        .read(|c| {
            c.query_row("SELECT rel_path FROM folders WHERE name='2024'", [], |r| {
                r.get(0)
            })
        })
        .unwrap();

    // c는 휴지통에 든 것처럼 — 원래 자리에 없어도 정상
    std::fs::remove_file(sub.join("b.jpg")).unwrap();
    std::fs::remove_file(sub.join("c.jpg")).unwrap();
    db.write(|c| c.execute("UPDATE files SET trashed_at = 1 WHERE name = 'c.jpg'", []))
        .unwrap();

    let n = prune_missing(&db, &mount, lib, &rel_dir).unwrap();
    assert_eq!(n, 1, "b만 지운다");
    let names: Vec<String> = db
        .read(|c| {
            let mut st = c.prepare("SELECT name FROM files ORDER BY name")?;
            let it = st.query_map([], |r| r.get(0))?;
            it.collect()
        })
        .unwrap();
    assert_eq!(names, vec!["a.jpg", "c.jpg"]);
    let cnt: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT file_count FROM folders WHERE name='2024'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(cnt, 1, "폴더 장수도 맞춘다 (휴지통 것은 안 센다)");

    // 아무것도 안 사라졌으면 0
    assert_eq!(prune_missing(&db, &mount, lib, &rel_dir).unwrap(), 0);
}

#[test]
fn missing_directory_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("db.sqlite")).unwrap();
    assert!(scan_folder(&db, 1, "/no/such/dir", 0, |_| {}).is_err());
}
