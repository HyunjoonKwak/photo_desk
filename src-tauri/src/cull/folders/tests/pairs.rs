use super::super::apply::apply_set;
use super::super::compare::PairRow;
use super::*;

/// a/ (작업대): 사본 둘 + 혼자인 것 하나 + T7끼리만 겹치는 둘.
/// b/ (공용): 원본 하나. c/ (공용): b 와 겹치는 것 하나 — 정착 구역 안 겹침.
fn setup_pair() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a"),
        dir.path().join("b"),
        dir.path().join("c"),
    );
    for d in [&a, &b, &c] {
        std::fs::create_dir_all(d).unwrap();
    }
    let same = b"SAME CONTENT ".repeat(100);
    let inner = b"INNER ONLY ".repeat(100);
    let pair = b"PAIR IN NAS ".repeat(100);
    std::fs::write(a.join("20200101_120000.jpg"), &same).unwrap();
    std::fs::write(a.join("copy.jpg"), &same).unwrap();
    std::fs::write(a.join("alone.jpg"), b"unique").unwrap();
    std::fs::write(a.join("20200102_120000.jpg"), &inner).unwrap();
    std::fs::write(a.join("inner-copy.jpg"), &inner).unwrap();
    std::fs::write(b.join("20200101_120001.jpg"), &same).unwrap();
    std::fs::write(b.join("20200103_120000.jpg"), &pair).unwrap();
    std::fs::write(c.join("20200103_120001.jpg"), &pair).unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 1, |_| {}).unwrap();
    db.write(|cn| {
        cn.execute("UPDATE folders SET area = 0", [])?;
        cn.execute(
            "UPDATE folders SET area = 2 WHERE rel_path LIKE '%b' OR rel_path LIKE '%c'",
            [],
        )
    })
    .unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    (dir, db)
}

#[test]
fn pair_apply_keeps_one_folder_and_marks_the_other() {
    let (_d, db) = setup_pair();
    let (fb, fc): (i64, i64) = db
        .read(|c| {
            Ok((
                c.query_row("SELECT id FROM folders WHERE rel_path LIKE '%b'", [], |r| {
                    r.get(0)
                })?,
                c.query_row("SELECT id FROM folders WHERE rel_path LIKE '%c'", [], |r| {
                    r.get(0)
                })?,
            ))
        })
        .unwrap();
    // 먼저 세어 보기 — 아무것도 안 바꾼다
    let dry = db.transaction(|tx| apply_pair(tx, fc, fb, true)).unwrap();
    assert_eq!((dry.groups, dry.kept, dry.rejected), (1, 1, 1));
    let untouched: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE culling_flag <> 0",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(untouched, 0, "dry_run 은 판정을 안 바꾼다");
    // c 를 남기고 b 것에 표시 — 대표가 b 였어도 뒤집힌다
    let r = db.transaction(|tx| apply_pair(tx, fc, fb, false)).unwrap();
    assert_eq!((r.groups, r.kept, r.rejected), (1, 1, 1));
    let flag: i32 = db
        .read(|c| {
            c.query_row(
                "SELECT culling_flag FROM files WHERE name = '20200103_120000.jpg'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(flag, 2, "b 의 것이 제외 표시");
    assert!(
        db.transaction(|tx| apply_pair(tx, fb, fb, false)).is_err(),
        "같은 폴더끼리는 거절"
    );
}

#[test]
fn unapply_clears_marks_and_reopens_groups() {
    let (_d, db) = setup();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    let s = &sets[0];
    let keep = s.folders[0].folder_id;
    let drops: Vec<i64> = s.folders[1..].iter().map(|f| f.folder_id).collect();
    db.transaction(|tx| apply_set(tx, keep, &drops)).unwrap();
    let marked: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE culling_flag <> 0",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(marked, 6);
    let all: Vec<i64> = std::iter::once(keep).chain(drops.iter().copied()).collect();
    let (files, groups) = db.transaction(|tx| unapply_folders(tx, &all)).unwrap();
    assert_eq!(
        (files, groups),
        (6, 1),
        "여섯 장 미판정으로, 닫았던 무리 하나 다시 연다"
    );
    let marked: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE culling_flag <> 0",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(marked, 0);
    let again = db.read(|c| identical_sets(c, 100)).unwrap();
    assert!(again[0].pending, "묶음이 다시 미결이 된다");
    assert_eq!(
        db.transaction(|tx| unapply_folders(tx, &[])).unwrap(),
        (0, 0)
    );
}

/// «남김»은 결정 — 앞선 짝에서 붙은 남김이 있는 폴더는 다시 제외되지 않고, 비교 화면도
/// 그쪽을 제외 후보로 올리지 않는다(kept_a/kept_b)
#[test]
fn a_kept_tree_is_not_demoted_and_not_offered() {
    let (_d, db) = setup();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    let s = &sets[0];
    let (c_id, a_id, b_id) = (
        s.folders[0].folder_id,
        s.folders[1].folder_id,
        s.folders[2].folder_id,
    );
    // 1) a 를 남기고 b 를 제외 → a 의 두 장에 «남김»
    db.transaction(|tx| apply_trees(tx, &[a_id], &[b_id]))
        .unwrap();
    let kept: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE folder_id = ?1 AND culling_flag = 1",
                [a_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(kept, 2);
    // 2) c 를 남기고 a 를 제외하려 해도 a 의 «남김»은 내려가지 않는다
    let r = db
        .transaction(|tx| apply_trees(tx, &[c_id], &[a_id]))
        .unwrap();
    assert_eq!(r.rejected, 0, "{r:?}");
    // 비교 화면도 a 쪽을 후보로 안 올린다 — kept_a 가 보인다
    let (vol, a_rel, c_rel): (String, String, String) = db
        .read(|c| {
            let vol: String = c.query_row(
                "SELECT volume_uuid FROM folders WHERE id = ?1",
                [a_id],
                |r| r.get(0),
            )?;
            let a_rel: String =
                c.query_row("SELECT rel_path FROM folders WHERE id = ?1", [a_id], |r| {
                    r.get(0)
                })?;
            let c_rel: String =
                c.query_row("SELECT rel_path FROM folders WHERE id = ?1", [c_id], |r| {
                    r.get(0)
                })?;
            Ok((vol, a_rel, c_rel))
        })
        .unwrap();
    let rows = db
        .read(|c| compare_two(c, (&vol, &a_rel), (&vol, &c_rel)))
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    // 2)에서 c 가 남는 쪽이 됐으니 c 에도 «남김» — 양쪽 다 남김이면 어느 쪽도 후보가 아니다
    assert!(
        rows[0].same && rows[0].kept_a == 2 && rows[0].kept_b == 2,
        "{:?}",
        rows[0]
    );
}

#[test]
fn pair_photos_link_identical_photos_one_to_one() {
    let (_d, db) = setup();
    let ids = |name: &str| -> i64 {
        db.read(|c| {
            c.query_row(
                "SELECT id FROM folders WHERE rel_path LIKE ?1",
                [format!("%{name}")],
                |r| r.get(0),
            )
        })
        .unwrap()
    };
    let (a, d) = (ids("a"), ids("d"));
    let p = db.read(|c| pair_photos(c, &[a], &[d])).unwrap();
    assert_eq!((p.a.len(), p.b.len()), (2, 2));
    // a/1.jpg(x) ↔ d/1.jpg(x) 만 같다; a/2.jpg(y) 와 d/3.jpg(z) 는 짝이 없다
    let a1 = p.a.iter().find(|x| x.name == "1.jpg").unwrap();
    let d1 = p.b.iter().find(|x| x.name == "1.jpg").unwrap();
    assert_eq!(a1.twin, Some(d1.file_id));
    assert_eq!(d1.twin, Some(a1.file_id));
    assert!(p
        .a
        .iter()
        .find(|x| x.name == "2.jpg")
        .unwrap()
        .twin
        .is_none());
    assert!(p
        .b
        .iter()
        .find(|x| x.name == "3.jpg")
        .unwrap()
        .twin
        .is_none());
    assert!(
        p.a.iter().all(|x| x.sub.is_empty()),
        "뿌리 바로 아래면 sub 는 비어 있다"
    );
}

#[test]
fn pairs_apply_counts_failures_without_aborting_the_batch() {
    let (_d, db) = setup();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    let s = &sets[0];
    let (keep, drop) = (s.folders[0].folder_id, s.folders[1].folder_id);
    let r = db
        .transaction(|tx| apply_pairs(tx, &[(vec![keep], vec![keep]), (vec![keep], vec![drop])]))
        .unwrap();
    assert_eq!((r.applied, r.failed), (1, 1), "{r:?}");
    assert!(r.first_error.is_some());
    assert_eq!(r.rejected, 2);
}

/// 후보1번에만 «블로그» 하위 폴더가 더 있는 경우 — 후보2번 쪽은 전부 후보1번에 있으니
/// «B 쪽이 A 에 다 있음»으로 한 줄에 잡히고, 하위 폴더는 따로 안 나온다
#[test]
fn a_tree_that_contains_the_other_side_is_paired_whole() {
    let (dir, db) = setup();
    let blog = dir.path().join("a/블로그");
    std::fs::create_dir_all(&blog).unwrap();
    std::fs::write(blog.join("b1.jpg"), b"BLOG ".repeat(300)).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let root_of = |name: &str| -> (String, String, i64) {
        db.read(|c| {
            c.query_row(
                "SELECT volume_uuid, rel_path, id FROM folders WHERE rel_path LIKE ?1",
                [format!("%{name}")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap()
    };
    let (a, b) = (root_of("a"), root_of("b"));
    let rows = db
        .read(|c| compare_two(c, (&a.0, &a.1), (&b.0, &b.1)))
        .unwrap()
        .rows;
    assert_eq!(
        rows.len(),
        1,
        "하위 폴더 «블로그»는 제 줄로 안 나온다: {rows:?}"
    );
    let r = &rows[0];
    assert!(r.b_in_a && !r.a_in_b && !r.same, "{r:?}");
    assert_eq!((r.files_a, r.files_b), (3, 2), "A 는 나무째 3장");
    assert_eq!(r.a_ids.len(), 2, "A 쪽 폴더 행 둘(a, a/블로그)");
    assert_eq!(r.b_ids, vec![b.2]);
    // 거꾸로 견줘도 같은 판정 — 이번엔 «A 쪽이 B 에 다 있음»
    let rows = db
        .read(|c| compare_two(c, (&b.0, &b.1), (&a.0, &a.1)))
        .unwrap()
        .rows;
    assert!(rows[0].a_in_b && !rows[0].b_in_a);
    // 나무째 표시 — B(2장) 제외, A 쪽은 남김. «블로그»의 한 장은 B 에 없으니 제외 대상이 아니다
    let out = db
        .transaction(|tx| apply_trees(tx, &r.a_ids, &r.b_ids))
        .unwrap();
    assert_eq!((out.kept, out.rejected), (3, 2), "{out:?}");
    assert!(
        db.transaction(|tx| apply_trees(tx, &r.a_ids, &r.a_ids))
            .is_err(),
        "겹치는 나무는 거절"
    );
}

/// Finder 에서 지운 폴더의 행이 남아 있어도 «없는 폴더»를 읽지 않는다
#[test]
fn folders_deleted_on_disk_are_left_out_and_counted() {
    let (dir, db) = setup();
    let root_of = |name: &str| -> (String, String) {
        db.read(|c| {
            c.query_row(
                "SELECT volume_uuid, rel_path FROM folders WHERE rel_path LIKE ?1",
                [format!("%{name}")],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap()
    };
    let (a, b) = (root_of("a"), root_of("b"));
    std::fs::remove_dir_all(dir.path().join("b")).unwrap(); // DB 행은 그대로
    let r = db
        .read(|c| compare_two(c, (&a.0, &a.1), (&b.0, &b.1)))
        .unwrap();
    assert_eq!(r.missing, 1, "{r:?}");
    assert!(
        r.rows.iter().all(|row| row.b.is_none()),
        "사라진 B 는 짝이 되지 않는다: {:?}",
        r.rows
    );
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    assert!(
        sets.iter()
            .all(|s| s.folders.iter().all(|f| f.folder != "b")),
        "폴더 비교도 뺀다: {sets:?}"
    );
}

/// 실제 DB 로 — `ACUT_LIVE_DB=<acut-v2.db> cargo test --lib real_db_compare -- --ignored --nocapture`
/// (앱이 열어 둔 DB 도 읽기 전용으로 열린다)
#[test]
#[ignore = "실제 DB"]
fn real_db_compare_missing() {
    let Ok(path) = std::env::var("ACUT_LIVE_DB") else {
        return;
    };
    let c = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let vol: String = c
            .query_row("SELECT volume_uuid FROM libraries WHERE name IN ('통합전후보', '로컬사진통합자료') LIMIT 1", [], |r| r.get(0))
            .unwrap_or_default();
    if std::env::var_os("ACUT_SETS").is_some() {
        let sets = identical_sets(&c, 5000).unwrap();
        let pending: Vec<&FolderSet> = sets.iter().filter(|s| s.pending).collect();
        eprintln!("sets {} pending {}", sets.len(), pending.len());
        for s in pending.iter().take(12) {
            let names: Vec<String> = s
                .folders
                .iter()
                .map(|f| {
                    format!(
                        "{}·{}{}",
                        f.library,
                        f.folder,
                        if f.area == 1 || f.area == 2 {
                            "(NAS)"
                        } else {
                            ""
                        }
                    )
                })
                .collect();
            let kept: i64 = c
                    .query_row(
                        &format!("SELECT COUNT(*) FROM files WHERE folder_id IN ({}) AND trashed_at IS NULL AND culling_flag = 1", s.ids.iter().flatten().map(i64::to_string).collect::<Vec<_>>().join(",")),
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
            eprintln!(
                "  {}장 flagged {} kept {} | {}",
                s.files,
                s.flagged,
                kept,
                names.join(" ⇔ ")
            );
        }
        return;
    }
    let a_root = std::env::var("ACUT_A").unwrap_or_else(|_| "통합전후보/후보1번/연도별".into());
    let b_root = std::env::var("ACUT_B").unwrap_or_else(|_| "통합전후보/후보2번".into());
    let r = compare_two(&c, (&vol, &a_root), (&vol, &b_root)).unwrap();
    eprintln!(
        "A={a_root} B={b_root} rows {} missing {}",
        r.rows.len(),
        r.missing
    );
    // B 쪽을 지워도 되는데 아직 표시가 안 된 짝 — 왜 표시가 안 붙나
    let pending: Vec<&PairRow> = r
        .rows
        .iter()
        .filter(|x| x.b_in_a && x.b.is_some() && x.flagged_b < x.files_b)
        .collect();
    eprintln!("pending b_in_a {}", pending.len());
    let pend_a: Vec<&PairRow> = r
        .rows
        .iter()
        .filter(|x| x.a_in_b && x.a.is_some() && x.flagged_a < x.files_a)
        .collect();
    eprintln!("pending a_in_b {}", pend_a.len());
    for x in pend_a.iter().take(8) {
        let ids = x
            .a_ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let flags: String = c
                .prepare(&format!("SELECT culling_flag, COUNT(*) FROM files WHERE folder_id IN ({ids}) AND trashed_at IS NULL GROUP BY culling_flag"))
                .unwrap()
                .query_map([], |r| Ok(format!("{}×{}", r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .map(|v| v.unwrap())
                .collect::<Vec<_>>()
                .join(" ");
        eprintln!(
            "  A {} | files a/b {}/{} flagged_a {} | A 판정: {flags}",
            x.a.as_ref().unwrap().folder,
            x.files_a,
            x.files_b,
            x.flagged_a
        );
    }
    for x in pending.iter().take(5) {
        eprintln!(
            "  {} | files a/b {}/{} flagged {}/{} a_ids {} b_ids {} same {}",
            x.b.as_ref().unwrap().folder,
            x.files_a,
            x.files_b,
            x.flagged_a,
            x.flagged_b,
            x.a_ids.len(),
            x.b_ids.len(),
            x.same
        );
    }
    if std::env::var_os("ACUT_LIVE_WRITE").is_some() {
        if let Some(x) = pending.first() {
            let mut c2 = Connection::open(&path).unwrap();
            let tx = c2.transaction().unwrap();
            let out = apply_trees(&tx, &x.a_ids, &x.b_ids);
            eprintln!("apply_trees → {out:?}");
            let n: i64 = tx
                    .query_row(
                        &format!(
                            "SELECT COUNT(*) FROM files WHERE folder_id IN ({}) AND trashed_at IS NULL AND full_hash IN (SELECT full_hash FROM files WHERE folder_id IN ({}) AND trashed_at IS NULL)",
                            x.b_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","),
                            x.a_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
                        ),
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
            eprintln!(
                "B 파일 중 A 나무에 같은 해시가 있는 것: {n} / {}",
                x.files_b
            );
            drop(tx); // 되돌린다
        }
    }
}

#[test]
fn two_roots_pair_identical_and_partial_folders() {
    let (_d, db) = setup();
    let root_of = |name: &str| -> (String, String) {
        db.read(|c| {
            c.query_row(
                "SELECT volume_uuid, rel_path FROM folders WHERE rel_path LIKE ?1",
                [format!("%{name}")],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap()
    };
    let (a, b, d) = (root_of("a"), root_of("b"), root_of("d"));
    // a 와 b 는 내용이 같다 — 뿌리끼리도 짝이 된다
    let rows = db
        .read(|c| compare_two(c, (&a.0, &a.1), (&b.0, &b.1)))
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].same && rows[0].b_in_a && rows[0].a_in_b && rows[0].common == 2);
    // a 와 d 는 한 장만 겹친다 — 뿌리 이름은 다르지만 뿌리끼리는 sub 가 같다("")
    let rows = db
        .read(|c| compare_two(c, (&a.0, &a.1), (&d.0, &d.1)))
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(!rows[0].same);
    assert_eq!(
        (rows[0].common, rows[0].files_a, rows[0].files_b),
        (1, 2, 2)
    );
}
