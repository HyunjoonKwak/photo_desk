use super::apply::apply_set;
use super::compare::roots_overlap;
use super::*;
use crate::cull::dedup;
use crate::db::conn::Db;
use crate::scan::scan_test;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// a/, b/, c/ 는 내용이 같은 폴더(파일 이름은 달라도 된다). d/ 는 한 장이 다르다.
fn setup() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let x = b"X ".repeat(500);
    let y = b"Y ".repeat(700);
    for (d, names) in [
        ("a", ["1.jpg", "2.jpg"]),
        ("b", ["one.jpg", "two.jpg"]),
        ("c", ["1.jpg", "2.jpg"]),
    ] {
        let p = dir.path().join(d);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(names[0]), &x).unwrap();
        std::fs::write(p.join(names[1]), &y).unwrap();
    }
    let p = dir.path().join("d");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("1.jpg"), &x).unwrap();
    std::fs::write(p.join("3.jpg"), b"Z ".repeat(700)).unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 1, |_| {}).unwrap();
    db.write(|c| {
        c.execute("UPDATE folders SET area = 0", [])?;
        c.execute("UPDATE folders SET area = 2 WHERE rel_path LIKE '%c'", [])
    })
    .unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    (dir, db)
}

#[test]
fn finds_folders_with_identical_contents() {
    let (_d, db) = setup();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    assert_eq!(sets.len(), 1, "{sets:?}");
    let s = &sets[0];
    let names: Vec<&str> = s.folders.iter().map(|f| f.folder.as_str()).collect();
    assert_eq!(names, ["c", "a", "b"], "정착 구역(c)이 맨 앞");
    assert_eq!(s.files, 2);
    assert!(s.pending);
}

#[test]
fn applying_a_set_marks_the_other_folders_and_settles_their_groups() {
    let (_d, db) = setup();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    let s = &sets[0];
    let keep = s.folders[0].folder_id;
    let drops: Vec<i64> = s.folders[1..].iter().map(|f| f.folder_id).collect();
    let r = db.transaction(|tx| apply_set(tx, keep, &drops)).unwrap();
    assert_eq!((r.kept, r.rejected), (2, 4));
    // d/1.jpg 는 a·b·c 밖에도 있어 그 무리는 미결로 남는다; y(2.jpg) 무리는 확정
    assert_eq!(r.groups, 1, "{r:?}");
    let pending = db.read(|c| identical_sets(c, 100)).unwrap();
    assert!(!pending[0].pending, "처리한 묶음은 pending 이 아니다");
}

#[test]
fn ancestors_walk_up_to_the_root() {
    assert_eq!(ancestors("a/b/c"), ["a/b", "a", ""]);
    assert_eq!(ancestors("a"), [""]);
    assert!(ancestors("").is_empty());
}

#[test]
fn roots_that_contain_each_other_overlap() {
    assert!(roots_overlap(
        ("v", "통합전후보"),
        ("v", "통합전후보/후보1번")
    ));
    assert!(roots_overlap(
        ("v", "통합전후보/후보1번"),
        ("v", "통합전후보")
    ));
    assert!(roots_overlap(("v", "a"), ("v", "a")));
    assert!(
        roots_overlap(("v", ""), ("v", "x")),
        "볼륨 뿌리는 전부를 품는다"
    );
    assert!(
        !roots_overlap(("v", "후보1"), ("v", "후보10")),
        "이름 앞만 같은 것"
    );
    assert!(!roots_overlap(("v1", "a"), ("v2", "a")), "다른 볼륨");
}

/// 하위 폴더까지 똑같은 두 나무는 위 폴더 한 줄로만 나온다 — 위 폴더에 사진이 바로 없어도(가상 마디)
#[test]
fn identical_trees_are_reported_once_at_the_top() {
    let dir = tempfile::tempdir().unwrap();
    for root in ["P", "Q"] {
        for (sub, body) in [
            ("2016/x", "BBBB"),
            ("2016/y", "CCCC"),
            ("2016/z/deep", "DDDD"),
        ] {
            let p = dir.path().join(root).join(sub);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("a.jpg"), body.as_bytes().repeat(200)).unwrap();
        }
    }
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    assert_eq!(
        sets.len(),
        1,
        "P ≡ Q 한 줄뿐 — 2016·x·y·z/deep 은 따로 안 나온다: {sets:?}"
    );
    assert_eq!(sets[0].files, 3, "나무째 3장");
    assert_eq!(
        sets[0].ids[0].len(),
        3,
        "폴더 행 셋(x, y, z/deep) — P·2016·z 는 가상 마디"
    );
    let names: Vec<&str> = sets[0].folders.iter().map(|f| f.folder.as_str()).collect();
    assert_eq!(
        names,
        ["P", "Q"],
        "가장 위에서 한 번만, 제 후손은 같이 안 묶인다"
    );
}

#[test]
fn a_folder_with_subfolders_is_never_called_identical() {
    // a/ 안에 하위 폴더 a/inner/ 가 생기면 a 는 «바로 아래만 같다»일 뿐 — 묶지 않는다
    let (dir, db) = setup();
    let inner = dir.path().join("a/inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("q.jpg"), b"Q ".repeat(300)).unwrap();
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    assert_eq!(sets.len(), 1, "{sets:?}");
    let names: Vec<&str> = sets[0].folders.iter().map(|f| f.folder.as_str()).collect();
    assert!(
        !names.contains(&"a"),
        "하위 폴더가 있는 a 는 빠진다: {names:?}"
    );
    assert_eq!(names, ["c", "b"]);
}

#[test]
fn apply_set_only_drops_files_that_still_have_a_copy_in_the_kept_folder() {
    let (dir, db) = setup();
    let sets = db.read(|c| identical_sets(c, 100)).unwrap();
    let s = &sets[0];
    let keep = s.folders[0].folder_id; // c
    let drops: Vec<i64> = s.folders[1..].iter().map(|f| f.folder_id).collect();
    // 목록을 본 뒤 남길 폴더(c)에서 2.jpg 가 사라졌다(휴지통) — 그 내용은 이제 a·b 에만 있다
    std::fs::remove_file(dir.path().join("c/2.jpg")).unwrap();
    db.write(|c| {
        c.execute(
            "UPDATE files SET trashed_at = 1 WHERE folder_id = ?1 AND name = '2.jpg'",
            [keep],
        )
    })
    .unwrap();
    let r = db.transaction(|tx| apply_set(tx, keep, &drops)).unwrap();
    assert_eq!(
        r.rejected, 2,
        "a/1, b/one 만 지우기 표시 — 2.jpg 사본은 남는다: {r:?}"
    );
    let flagged: Vec<String> = db
        .read(|c| {
            let mut st =
                c.prepare("SELECT name FROM files WHERE culling_flag = 2 ORDER BY name")?;
            let it = st.query_map([], |r| r.get(0))?;
            it.collect()
        })
        .unwrap();
    assert_eq!(flagged, ["1.jpg", "one.jpg"]);
}

/// «2004» ⇔ «2004/주원이사진» — 바깥 뿌리에서 안쪽 나무를 빼고 견준다
#[test]
fn a_root_inside_the_other_is_excluded_from_the_outer_side() {
    let dir = tempfile::tempdir().unwrap();
    for (d, body) in [
        ("2004/2004-09-08", "AAAA"),
        ("2004/2004-09-09", "BBBB"),
        ("2004/주원이사진/2004-09-08", "AAAA"),
        ("2004/주원이사진/2004-09-09", "BBBB"),
        ("2004/주원이사진/2004-09-10", "CCCC"),
    ] {
        let p = dir.path().join(d);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("a.jpg"), body.as_bytes().repeat(200)).unwrap();
    }
    let db = Db::open(dir.path().join("t.db")).unwrap();
    scan_test(&db, dir.path(), 2, |_| {}).unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let (vol, lib_rel): (String, String) = db
        .read(|c| {
            c.query_row("SELECT volume_uuid, rel_path FROM libraries", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })
        .unwrap();
    let j = |s: &str| {
        if lib_rel.is_empty() {
            s.to_string()
        } else {
            format!("{lib_rel}/{s}")
        }
    };
    // A = 주원이사진(남길 쪽), B = 2004(바깥) — B 쪽에서 주원이사진 아래는 빠진다
    let r = db
        .read(|c| compare_two(c, (&vol, &j("2004/주원이사진")), (&vol, &j("2004"))))
        .unwrap();
    let same: Vec<String> = r
        .rows
        .iter()
        .filter(|x| x.same)
        .map(|x| x.b.as_ref().unwrap().folder.clone())
        .collect();
    assert_eq!(same.len(), 2, "09-08·09-09 짝: {:?}", r.rows);
    assert!(
        same.iter().all(|f| !f.contains("주원이사진")),
        "B 쪽에 주원이사진 아래 폴더가 섞이면 안 된다: {same:?}"
    );
    // 09-10 은 A 에만 있다 — «A 에만 있음» 줄로, A 쪽이 B 에 다 있는 짝(똑같음 말고)은 없다
    assert!(
        r.rows.iter().any(|x| x.a.is_some() && x.b.is_none()),
        "{:?}",
        r.rows
    );
    assert!(r.rows.iter().all(|x| !x.a_in_b || x.same), "{:?}", r.rows);
    // 뿌리가 같으면 거절
    assert!(db
        .read(|c| compare_two(c, (&vol, &j("2004")), (&vol, &j("2004"))))
        .is_err());
}

#[test]
fn compare_two_rejects_overlapping_roots_and_lists_in_path_order() {
    let (dir, db) = setup();
    // 뿌리 아래 여러 폴더: root/{a,b,c,d} 를 통째로 다른 곳과 견주려면 뿌리가 둘 필요 —
    // 여기서는 «뿌리(전체) 대 a» 가 겹친다는 것만 본다
    let vol: String = db
        .read(|c| c.query_row("SELECT volume_uuid FROM folders LIMIT 1", [], |r| r.get(0)))
        .unwrap();
    let a_rel: String = db
        .read(|c| {
            c.query_row(
                "SELECT rel_path FROM folders WHERE rel_path LIKE '%a'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    let root_rel = a_rel
        .rsplit_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_default();
    // 품는 관계는 이제 허용 — 바깥에서 안쪽을 빼고 견준다 (같은 뿌리만 거절)
    assert!(db
        .read(|c| compare_two(c, (&vol, &root_rel), (&vol, &a_rel)))
        .is_ok());
    // 경로 순: 두 뿌리 아래 폴더가 여럿일 때 sub 오름차순 — x/{1,2} 대 y/{2,1}
    for (d, names) in [
        ("x/1", ["p.jpg"]),
        ("x/2", ["p.jpg"]),
        ("y/1", ["p.jpg"]),
        ("y/2", ["p.jpg"]),
    ] {
        let p = dir.path().join(d);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join(names[0]), d.as_bytes().repeat(200)).unwrap();
    }
    scan_test(&db, dir.path(), 0, |_| {}).unwrap();
    dedup::scan(&db, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
    let x = format!("{root_rel}/x").trim_start_matches('/').to_string();
    let y = format!("{root_rel}/y").trim_start_matches('/').to_string();
    let rows = db
        .read(|c| compare_two(c, (&vol, &x), (&vol, &y)))
        .unwrap()
        .rows;
    let subs: Vec<String> = rows
        .iter()
        .map(|r| r.a.as_ref().or(r.b.as_ref()).unwrap().folder.clone())
        .collect();
    let mut sorted = subs.clone();
    sorted.sort();
    assert_eq!(subs, sorted, "경로 오름차순: {subs:?}");
    assert!(
        rows.iter().all(|r| !r.same),
        "내용이 다 다르니 같은 짝은 없다"
    );
}

mod pairs;
