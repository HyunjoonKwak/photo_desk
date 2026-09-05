use super::*;
use crate::ops::organize::{move_to, Dest};
use crate::ops::trash;
use crate::scan::scan_test;

fn setup() -> (tempfile::TempDir, Db, i64, Vec<i64>) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("작업대");
    std::fs::create_dir_all(&src).unwrap();
    for n in ["a.jpg", "b.jpg"] {
        std::fs::write(src.join(n), b"bytes ".repeat(20)).unwrap();
    }
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    let lib: i64 = db
        .read(|c| c.query_row("SELECT id FROM libraries", [], |r| r.get(0)))
        .unwrap();
    let ids: Vec<i64> = db
        .read(|c| {
            let mut st = c.prepare("SELECT id FROM files ORDER BY name")?;
            let it = st.query_map([], |r| r.get(0))?;
            it.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap();
    (dir, db, lib, ids)
}

#[test]
fn undo_puts_moved_files_back() {
    let (dir, db, lib, ids) = setup();
    let dest = Dest {
        library_id: lib,
        rel_dir: "2024/행사".into(),
    };
    let out = move_to(&db, &ids, &dest, "정리").unwrap();
    assert!(!dir.path().join("작업대/a.jpg").exists());

    let u = undo(&db, out.batch_id).unwrap();
    assert_eq!((u.moved, u.failed), (2, 0));
    assert!(
        dir.path().join("작업대/a.jpg").is_file(),
        "원래 자리로 돌아온다"
    );
    assert!(!dir.path().join("2024/행사/a.jpg").exists());

    // rel_path는 볼륨 기준이라 임시 폴더에서는 앞이 길다. 끝만 본다.
    let rel: String = db
        .read(|c| {
            c.query_row(
                "SELECT fo.rel_path FROM files fi JOIN folders fo ON fo.id=fi.folder_id
                     WHERE fi.id=?1",
                [ids[0]],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(rel.ends_with("작업대"), "DB도 함께 돌아온다: {rel}");
}

#[test]
fn undo_of_a_trash_batch_restores_the_flag_too() {
    let (dir, db, _lib, ids) = setup();
    let out = trash::to_trash(&db, &ids[..1], "치우기").unwrap();
    let trashed: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE trashed_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(trashed, 1);

    undo(&db, out.batch_id).unwrap();
    assert!(dir.path().join("작업대/a.jpg").is_file());
    let still: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE trashed_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(still, 0, "휴지통 표시도 지워져야 목록에 다시 나온다");
}

#[test]
fn journal_keeps_the_destination_volume_apart_from_the_source() {
    let (_dir, db, _lib, ids) = setup();
    let batch = crate::ops::open_batch(&db, "move", "볼륨 넘어가기").unwrap();
    crate::ops::record_to(
        &db,
        batch,
        "move",
        ids[0],
        "VOL-A",
        "a/x.jpg",
        "VOL-B",
        Some("b/x.jpg"),
        Ok(()),
    )
    .unwrap();
    let (from, to): (String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT from_vol, to_vol FROM journal WHERE batch_id = ?1",
                [batch],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(
        (from.as_str(), to.as_str()),
        ("VOL-A", "VOL-B"),
        "to_vol 이 from_vol 에 묶이지 않는다"
    );
}

#[test]
fn a_trash_undo_that_moved_nothing_stays_undoable() {
    let (dir, db, _lib, ids) = setup();
    let out = trash::to_trash(&db, &ids[..1], "치우기").unwrap();
    // 휴지통의 파일이 그새 사라졌다 — 되돌릴 것이 없다
    let trash_path: String = db
        .read(|c| {
            c.query_row(
                "SELECT trash_path FROM files WHERE id = ?1",
                [ids[0]],
                |r| r.get(0),
            )
        })
        .unwrap();
    let _ = std::fs::remove_file(dir.path().join(&trash_path));
    let _ = std::fs::remove_file(&trash_path);
    let u = undo(&db, out.batch_id).unwrap();
    let undone: Option<i64> = db
        .read(|c| {
            c.query_row(
                "SELECT undone_at FROM batches WHERE id=?1",
                [out.batch_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    if u.moved == 0 {
        assert!(
            undone.is_none(),
            "하나도 못 돌렸으면 배치는 열려 있어야 한다"
        );
    } else {
        assert!(undone.is_some());
    }
}

#[test]
fn undoing_a_trash_batch_after_the_trash_view_restored_it_just_closes_it() {
    let (dir, db, _lib, ids) = setup();
    let t = trash::to_trash(&db, &ids[..1], "휴지통으로").unwrap();
    trash::restore(&db, &ids[..1]).unwrap(); // 휴지통 화면에서 되돌렸다
    let u = undo(&db, t.batch_id).unwrap();
    assert_eq!((u.moved, u.failed), (0, 0));
    assert!(u.first_error.as_deref().unwrap_or("").contains("이미"));
    let undone: Option<i64> = db
        .read(|c| {
            c.query_row(
                "SELECT undone_at FROM batches WHERE id=?1",
                [t.batch_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(
        undone.is_some(),
        "할 일이 없는 배치는 닫힌다 — 단추가 영영 남지 않게"
    );
    assert!(dir.path().join("작업대/a.jpg").is_file(), "사진은 제자리");
}

#[test]
fn undoing_a_restore_puts_the_photo_back_into_the_trash() {
    let (dir, db, _lib, ids) = setup();
    trash::to_trash(&db, &ids[..1], "휴지통으로").unwrap();
    let r = trash::restore(&db, &ids[..1]).unwrap();
    assert!(dir.path().join("작업대/a.jpg").is_file());
    let u = undo(&db, r.batch_id).unwrap();
    assert_eq!((u.moved, u.failed), (1, 0), "{u:?}");
    assert!(!dir.path().join("작업대/a.jpg").exists(), "다시 휴지통으로");
    let trashed: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM files WHERE trashed_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(trashed, 1);
}

#[test]
fn empty_operations_do_not_leave_batches_behind() {
    let (_d, db, _lib, ids) = setup();
    let before: i64 = db
        .read(|c| c.query_row("SELECT COUNT(*) FROM batches", [], |r| r.get(0)))
        .unwrap();
    let r = trash::restore(&db, &ids).unwrap(); // 휴지통이 비었다
    assert_eq!(r.moved, 0);
    assert!(r.first_error.is_some());
    let after: i64 = db
        .read(|c| c.query_row("SELECT COUNT(*) FROM batches", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(before, after, "빈 배치가 생기지 않는다");
}

#[test]
fn a_permanent_delete_cannot_be_undone() {
    let (dir, db, _lib, ids) = setup();
    trash::to_trash(&db, &ids[..1], "휴지통으로").unwrap();
    let e = trash::empty(&db, dir.path(), &ids[..1]).unwrap();
    let u = undo(&db, e.batch_id).unwrap();
    assert_eq!(u.moved, 0);
    assert!(u
        .first_error
        .as_deref()
        .unwrap_or("")
        .contains("되돌릴 수 없"));
    let undone: Option<i64> = db
        .read(|c| {
            c.query_row(
                "SELECT undone_at FROM batches WHERE id=?1",
                [e.batch_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(
        undone.is_none(),
        "«되돌린 작업»으로 꾸미지 않는다 — 지운 건 지운 것"
    );
}

#[test]
fn undoing_twice_does_nothing() {
    let (_d, db, lib, ids) = setup();
    let dest = Dest {
        library_id: lib,
        rel_dir: "2024/행사".into(),
    };
    let out = move_to(&db, &ids, &dest, "정리").unwrap();
    undo(&db, out.batch_id).unwrap();
    let again = undo(&db, out.batch_id).unwrap();
    assert_eq!(again.moved, 0);
    assert!(again.first_error.is_some());
}

#[test]
fn recent_closes_trash_batches_that_have_nothing_left_to_undo() {
    let (_d, db, _lib, ids) = setup();
    let t = trash::to_trash(&db, &ids[..1], "휴지통으로").unwrap();
    assert!(recent(&db, 10)
        .unwrap()
        .iter()
        .any(|b| b.id == t.batch_id && b.undone_at.is_none()));
    trash::restore(&db, &ids[..1]).unwrap(); // 휴지통 화면에서 되돌림
    let list = recent(&db, 10).unwrap();
    let b = list.iter().find(|b| b.id == t.batch_id).unwrap();
    assert!(
        b.undone_at.is_some(),
        "물릴 게 없는 묶음은 목록을 읽을 때 닫힌다"
    );
}

#[test]
fn recent_lists_newest_first_and_shows_undone() {
    let (_d, db, lib, ids) = setup();
    let a = move_to(
        &db,
        &ids[..1],
        &Dest {
            library_id: lib,
            rel_dir: "x".into(),
        },
        "1",
    )
    .unwrap();
    let b = move_to(
        &db,
        &ids[1..],
        &Dest {
            library_id: lib,
            rel_dir: "y".into(),
        },
        "2",
    )
    .unwrap();
    undo(&db, a.batch_id).unwrap();

    let list = recent(&db, 10).unwrap();
    assert_eq!(list[0].id, b.batch_id, "최근 것이 위");
    let first = list.iter().find(|x| x.id == a.batch_id).unwrap();
    assert!(first.undone_at.is_some(), "되돌린 표시가 남는다");
}

/// 정리한 뒤 외부에서 같은 이름으로 교체된 파일은 원래 자리로 가져가지 않는다
#[test]
fn undo_refuses_a_file_replaced_after_the_move() {
    let (dir, db, lib, ids) = setup();
    let dest = Dest {
        library_id: lib,
        rel_dir: "2024/행사".into(),
    };
    let out = move_to(&db, &ids, &dest, "정리").unwrap();
    let moved = dir.path().join("2024/행사/a.jpg");
    std::fs::write(&moved, b"a different photo altogether").unwrap();

    let u = undo(&db, out.batch_id).unwrap();
    assert_eq!((u.moved, u.failed), (1, 1), "{:?}", u.first_error);
    assert_eq!(u.failed_ids, vec![ids[0]]);
    assert!(u
        .first_error
        .as_deref()
        .unwrap_or_default()
        .contains("바뀌어"));
    assert!(moved.is_file(), "바뀐 파일은 그 자리에 둔다");
    assert!(!dir.path().join("작업대/a.jpg").exists());
    assert!(
        dir.path().join("작업대/b.jpg").is_file(),
        "안 바뀐 것은 돌아온다"
    );
    let undone: Option<i64> = db
        .read(|c| {
            c.query_row(
                "SELECT undone_at FROM batches WHERE id=?1",
                [out.batch_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(undone.is_none(), "남은 항목이 있으니 묶음은 열어 둔다");
}

/// 휴지통 화면에서 영구히 비운 «휴지통으로» 묶음 — «이미 되돌렸다»가 아니라 «지웠다»
#[test]
fn an_emptied_trash_batch_says_it_was_deleted_not_undone() {
    let (dir, db, _lib, ids) = setup();
    let t = trash::to_trash(&db, &ids, "치우기").unwrap();
    assert_eq!(t.moved, 2);
    let e = trash::empty(&db, dir.path(), &ids).unwrap();
    assert_eq!(e.moved, 2, "{:?}", e.first_error);
    recent(&db, 10).unwrap();
    let u = undo(&db, t.batch_id).unwrap();
    assert!(
        u.first_error
            .as_deref()
            .unwrap_or_default()
            .contains("영구히"),
        "{:?}",
        u.first_error
    );
}

#[test]
fn undo_does_not_overwrite_a_newer_file_in_the_old_place() {
    let (dir, db, lib, ids) = setup();
    let dest = Dest {
        library_id: lib,
        rel_dir: "2024/행사".into(),
    };
    let out = move_to(&db, &ids[..1], &dest, "정리").unwrap();
    // 그새 같은 이름의 새 사진이 원래 자리에 들어왔다
    std::fs::write(dir.path().join("작업대/a.jpg"), b"NEW PHOTO").unwrap();

    let u = undo(&db, out.batch_id).unwrap();
    assert_eq!((u.moved, u.failed), (1, 0));
    assert_eq!(
        std::fs::read(dir.path().join("작업대/a.jpg")).unwrap(),
        b"NEW PHOTO",
        "새 사진은 그대로"
    );
    assert!(
        dir.path().join("작업대/a (2).jpg").is_file(),
        "돌아온 것은 옆에 놓인다"
    );
    let name: String = db
        .read(|c| c.query_row("SELECT name FROM files WHERE id=?1", [ids[0]], |r| r.get(0)))
        .unwrap();
    assert_eq!(name, "a (2).jpg", "DB도 새 이름을 안다");
}

#[test]
fn a_fully_failed_undo_stays_undoable() {
    let (dir, db, lib, ids) = setup();
    let dest = Dest {
        library_id: lib,
        rel_dir: "2024/행사".into(),
    };
    let out = move_to(&db, &ids[..1], &dest, "정리").unwrap();
    // 옮긴 파일이 사라져 되돌릴 수 없다
    std::fs::remove_file(dir.path().join("2024/행사/a.jpg")).unwrap();
    let u = undo(&db, out.batch_id).unwrap();
    assert_eq!((u.moved, u.failed), (0, 1));
    let undone: Option<i64> = db
        .read(|c| {
            c.query_row(
                "SELECT undone_at FROM batches WHERE id=?1",
                [out.batch_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(
        undone.is_none(),
        "하나도 못 돌렸으면 «되돌린 것»으로 찍지 않는다"
    );
}
