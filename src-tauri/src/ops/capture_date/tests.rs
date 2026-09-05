use super::*;
use crate::scan::scan_test;

fn jpeg(path: &Path) {
    image::RgbImage::from_pixel(8, 8, image::Rgb([20, 40, 60]))
        .save(path)
        .unwrap();
}

#[test]
fn dry_run_does_not_change_file_or_db_and_write_rescan_undo_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("사진");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("20240102_235958.jpg");
    jpeg(&path);
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, &root, 0, |_| {}).unwrap();
    let id: i64 = db
        .read(|c| c.query_row("SELECT id FROM files", [], |r| r.get(0)))
        .unwrap();
    let before = sha256(&path).unwrap();
    let rows = audit(
        &db,
        &AuditTarget {
            ids: vec![id],
            library_id: None,
            rel_path: None,
            recursive: true,
        },
    )
    .unwrap();
    assert_eq!(sha256(&path).unwrap(), before);
    assert!(rows[0].auto_selected);
    let wanted = rows[0].proposed_at.unwrap();
    let out = apply(
        &db,
        &[Change {
            id,
            taken_at: wanted,
            manual: false,
        }],
        "시험",
    )
    .unwrap();
    assert_eq!((out.corrected, out.failed), (1, 0));
    assert_eq!(out.manifest[0].rescan_at, wanted);
    assert_ne!(sha256(&path).unwrap(), before);
    let undone = undo(&db, out.batch_id).unwrap();
    assert_eq!((undone.moved, undone.failed), (1, 0));
    assert_eq!(
        sha256(&path).unwrap(),
        before,
        "undo는 원본 바이트까지 복원"
    );
}

#[test]
fn partial_failure_keeps_success_journal_and_non_jpeg_scope_is_honest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("사진");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("20240102_120000.png"), b"not really png").unwrap();
    std::fs::write(root.join("20240102_120001.png"), b"also bytes").unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, &root, 0, |_| {}).unwrap();
    let ids: Vec<i64> = db
        .read(|c| {
            let mut s = c.prepare("SELECT id FROM files ORDER BY name")?;
            let rows = s.query_map([], |r| r.get(0))?.collect();
            rows
        })
        .unwrap();
    std::fs::remove_file(root.join("20240102_120000.png")).unwrap();
    let t = taken_at::civil_to_unix(2024, 1, 3, 0, 0, 1);
    let out = apply(
        &db,
        &[
            Change {
                id: ids[0],
                taken_at: t,
                manual: true,
            },
            Change {
                id: ids[1],
                taken_at: t,
                manual: true,
            },
        ],
        "부분",
    )
    .unwrap();
    assert_eq!((out.corrected, out.failed), (1, 1));
    assert_eq!(out.manifest[0].write_scope, "mtime+desk-override");
    assert_eq!(undo(&db, out.batch_id).unwrap().moved, 1);
}

#[test]
fn undo_refuses_to_overwrite_a_file_changed_after_correction() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("사진");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("20240102_120000.jpg");
    jpeg(&path);
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, &root, 0, |_| {}).unwrap();
    let id: i64 = db
        .read(|c| c.query_row("SELECT id FROM files", [], |r| r.get(0)))
        .unwrap();
    let out = apply(
        &db,
        &[Change {
            id,
            taken_at: taken_at::civil_to_unix(2024, 1, 3, 12, 0, 0),
            manual: true,
        }],
        "교정",
    )
    .unwrap();

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"later edit")
        .unwrap();
    let changed = sha256(&path).unwrap();
    let undone = undo(&db, out.batch_id).unwrap();
    assert_eq!((undone.moved, undone.failed), (0, 1));
    assert_eq!(sha256(&path).unwrap(), changed);
    assert!(undone
        .first_error
        .as_deref()
        .unwrap_or_default()
        .contains("바뀌어"));
}
