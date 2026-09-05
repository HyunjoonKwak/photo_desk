use super::*;
use crate::cull::hash::fixtures::{jpeg, seg};
use crate::scan::scan_test;

/// 4단계 시험판 — 같은 그림에 촬영일시 EXIF 만 써 넣은 사본, 다른 그림, 너무 많이 다른 사본
fn twin_setup() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    for d in ["mine", "t7", "other", "fat"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let scan = [0x12, 0x34, 0xFF, 0x00, 0x56, 0x78];
    // 내사진 쪽: 날짜 EXIF 가 붙어 106바이트 더 크다
    let exif = seg(0xE1, &[b'E'; 102]);
    std::fs::write(dir.path().join("mine/IMG_1.jpg"), jpeg(&[exif], &scan)).unwrap();
    // T7 쪽: 머리 없는 원본
    std::fs::write(dir.path().join("t7/IMG_1.jpg"), jpeg(&[], &scan)).unwrap();
    // 이름·크기(픽셀)는 같지만 다른 그림
    std::fs::write(
        dir.path().join("other/IMG_1.jpg"),
        jpeg(&[], &[0x12, 0x34, 0xFF, 0x00, 0x00]),
    )
    .unwrap();
    // 같은 그림이지만 크기 차가 상한을 넘는다 — 재지 않는다
    let fat = seg(0xFE, &vec![b'x'; TWIN_SLACK as usize + 10]);
    std::fs::write(dir.path().join("fat/IMG_1.jpg"), jpeg(&[fat], &scan)).unwrap();

    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 1, |_| {}).unwrap();
    // 시험판 JPEG 은 실제 그림이 아니라 스캐너가 픽셀 크기를 못 읽는다 — 같은 크기로 채운다
    db.write(|c| c.execute("UPDATE files SET width=16, height=16", []))
        .unwrap();
    (dir, db)
}

#[test]
fn a_copy_that_only_differs_in_metadata_is_grouped() {
    let (_d, db) = twin_setup();
    let p = scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    assert_eq!(p.groups, 1);
    let (reason, n): (String, i64) = db
            .read(|c| {
                c.query_row(
                    "SELECT g.reason, COUNT(*) FROM groups g JOIN group_members m ON m.group_id=g.id GROUP BY g.id",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap();
    assert_eq!(reason, "메타데이터만 다름");
    assert_eq!(n, 2, "mine 과 t7 만 — 다른 그림·상한 밖 사본은 빠진다");
    // 그림 해시가 남아 다음엔 다시 읽지 않는다 (상한 밖 사본은 재지 않았다)
    let hashed: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE image_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(hashed, 3);
    // 확보 용량은 대표를 뺀 나머지 크기
    let (gain, best_size): (i64, i64) = db
        .read(|c| {
            c.query_row(
                "SELECT g.size_bytes, f.size FROM groups g JOIN group_members m ON m.group_id=g.id
                     JOIN files f ON f.id=m.file_id WHERE m.is_best=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    let total: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT SUM(f.size) FROM group_members m JOIN files f ON f.id=m.file_id",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(gain, total - best_size);
}

#[test]
fn byte_identical_and_metadata_twins_fold_into_one_group() {
    let (d, db) = twin_setup();
    // t7 원본과 바이트까지 같은 사본 하나 더 — 세 장이 한 그룹, 사유는 «메타데이터만 다름»
    std::fs::create_dir_all(d.path().join("t7b")).unwrap();
    std::fs::copy(
        d.path().join("t7/IMG_1.jpg"),
        d.path().join("t7b/IMG_1.jpg"),
    )
    .unwrap();
    scan_test(&db, d.path(), 1, |_| {}).unwrap();
    db.write(|c| c.execute("UPDATE files SET width=16, height=16", []))
        .unwrap();
    let p = scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    assert_eq!(p.groups, 1);
    let (reason, n): (String, i64) = db
            .read(|c| {
                c.query_row(
                    "SELECT g.reason, COUNT(*) FROM groups g JOIN group_members m ON m.group_id=g.id GROUP BY g.id",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap();
    assert_eq!((reason.as_str(), n), ("메타데이터만 다름", 3));
}

#[test]
fn merge_groups_joins_through_shared_members() {
    let got = merge_groups(
        vec![vec![1, 2], vec![5, 6]].into_iter(),
        vec![vec![2, 3], vec![9, 10]].into_iter(),
    );
    assert_eq!(got, vec![vec![1, 2, 3], vec![5, 6], vec![9, 10]]);
}

/// 같은 내용의 파일을 여러 개 만들어 스캔한다.
fn setup() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();

    // 같은 내용 3개 (경로·이름은 다르다)
    for (d, n) in [
        (&a, "20200101_120000.jpg"),
        (&b, "20200101_120001.jpg"),
        (&a, "copy.jpg"),
    ] {
        std::fs::write(d.join(n), b"SAME CONTENT ".repeat(100)).unwrap();
    }
    // 크기는 같지만 내용이 다른 것 — 그룹에 들어가면 안 된다
    std::fs::write(a.join("other.jpg"), b"DIFF CONTENT ".repeat(100)).unwrap();
    // 혼자인 것
    std::fs::write(b.join("alone.jpg"), b"unique").unwrap();

    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 1, |_| {}).unwrap();
    (dir, db)
}

#[test]
fn groups_only_byte_identical_files() {
    let (_d, db) = setup();
    let p = scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    assert_eq!(p.groups, 1, "같은 내용 3개가 한 그룹");

    let members: i64 = db
        .read(|c| c.query_row("SELECT COUNT(*) FROM group_members", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(members, 3);
}

/// 폴더 비교가 붙인 «남김»이 대표가 되고, 표시가 다 붙은 무리는 닫혀서 만들어진다
#[test]
fn a_kept_mark_wins_best_and_a_decided_group_is_created_closed() {
    let (_d, db) = setup();
    db.write(|c| {
        c.execute(
            "UPDATE files SET culling_flag = CASE name
                   WHEN '20200101_120001.jpg' THEN 1
                   WHEN '20200101_120000.jpg' THEN 2
                   WHEN 'copy.jpg' THEN 2 ELSE 0 END",
            [],
        )
    })
    .unwrap();
    let p = scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    assert_eq!(p.groups, 1);
    let (state, best_name): (i64, String) = db
        .read(|c| {
            c.query_row(
                "SELECT g.state, f.name FROM groups g
                     JOIN group_members m ON m.group_id = g.id AND m.is_best = 1
                     JOIN files f ON f.id = m.file_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(
        state, 1,
        "남김 하나 + 제외 둘 = 이미 결정 — 미결로 다시 묻지 않는다"
    );
    assert_eq!(
        best_name, "20200101_120001.jpg",
        "남김 표시가 대표를 이긴다"
    );
}

/// 표시가 일부만 있으면 미결로 남되, 남김이 대표가 된다
#[test]
fn a_partial_mark_keeps_the_group_open_with_the_kept_one_as_best() {
    let (_d, db) = setup();
    db.write(|c| {
        c.execute(
            "UPDATE files SET culling_flag = 1 WHERE name = 'copy.jpg'",
            [],
        )
    })
    .unwrap();
    scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let (state, best_name): (i64, String) = db
        .read(|c| {
            c.query_row(
                "SELECT g.state, f.name FROM groups g
                     JOIN group_members m ON m.group_id = g.id AND m.is_best = 1
                     JOIN files f ON f.id = m.file_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(state, 0);
    assert_eq!(best_name, "copy.jpg");
}

#[test]
fn same_size_different_content_is_not_a_duplicate() {
    let (_d, db) = setup();
    scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    // "other.jpg"는 크기가 같아 후보였지만 그룹에 없어야 한다
    let in_group: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM group_members gm
                     JOIN files f ON f.id = gm.file_id WHERE f.name='other.jpg'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(in_group, 0, "크기만 같은 것은 중복이 아니다");
}

#[test]
fn reclaimable_counts_all_but_one() {
    let (_d, db) = setup();
    let p = scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let one: i64 = db
        .read(|c| {
            c.query_row("SELECT size FROM files WHERE name='copy.jpg'", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(p.reclaimable, one * 2, "3개 중 2개분만 확보된다");
}

#[test]
fn exactly_one_best_per_group() {
    let (_d, db) = setup();
    scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let bests: i64 = db
        .read(|c| c.query_row("SELECT SUM(is_best) FROM group_members", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(bests, 1, "그룹마다 유지 후보는 하나");
}

/// 옛 백업(작업대)과 내사진에 같은 파일이 있으면, 촬영일이 늦어도 내사진 쪽이 유지본
#[test]
fn settled_copy_wins_over_an_earlier_shot_in_the_desk() {
    let (_d, db) = setup();
    db.transaction(|tx| {
        tx.execute("UPDATE folders SET area = 0 WHERE name = 'a'", [])?;
        tx.execute("UPDATE folders SET area = 1 WHERE name = 'b'", [])?;
        Ok(())
    })
    .unwrap();
    scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let best_name: String = db
            .read(|c| {
                c.query_row(
                    "SELECT f.name FROM group_members m JOIN files f ON f.id = m.file_id WHERE m.is_best = 1",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
    assert_eq!(
        best_name, "20200101_120001.jpg",
        "내사진(b)의 사본이 남는다"
    );
}

#[test]
fn full_hash_is_saved_for_reuse() {
    let (_d, db) = setup();
    scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let hashed: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE full_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(hashed >= 3, "다음 스캔에서 다시 읽지 않도록 저장한다");
    let quick: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE quick_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(
        quick >= hashed,
        "빠른 해시도 남긴다 — 전체 해시 대상은 빠른 해시를 거쳤다"
    );
}

#[test]
fn progress_reports_the_full_hash_phase() {
    let (_d, db) = setup();
    let phases = Mutex::new(Vec::new());
    scan(&db, Arc::new(AtomicBool::new(false)), |p| {
        phases
            .lock()
            .unwrap()
            .push((p.phase, p.full_total, p.full_done));
    })
    .unwrap();
    let ph = phases.into_inner().unwrap();
    assert!(
        ph.iter().any(|x| x.0 == "full" && x.1 >= 3),
        "전체 해시 단계를 알린다: {ph:?}"
    );
    assert!(
        ph.iter().any(|x| x.0 == "full" && x.2 == x.1 && x.1 > 0),
        "끝까지 센다: {ph:?}"
    );
}

#[test]
fn rerunning_does_not_duplicate_groups() {
    let (_d, db) = setup();
    let cancel = Arc::new(AtomicBool::new(false));
    scan(&db, cancel.clone(), |_| {}).unwrap();
    scan(&db, cancel, |_| {}).unwrap();
    let groups: i64 = db
        .read(|c| c.query_row("SELECT COUNT(*) FROM groups WHERE kind=0", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(groups, 1, "다시 돌려도 그룹이 쌓이지 않는다");
}

#[test]
fn cancellation_stops_early() {
    let (_d, db) = setup();
    let p = scan(&db, Arc::new(AtomicBool::new(true)), |_| {}).unwrap();
    assert_eq!(p.groups, 0);
}
