use super::*;

pub(super) fn seeded() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library')",
            [],
        )?;
        tx.execute(
            "INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(2,'V','a/b','b',1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(3,'V','z','z',2)",
            [],
        )?;
        // 같은 taken_at을 여럿 두어 동점 처리를 시험한다
        for i in 1..=50 {
            let folder = if i <= 30 {
                1
            } else if i <= 40 {
                2
            } else {
                3
            };
            let taken = 1_000_000 + (i / 5) * 100; // 5개씩 같은 시각
            tx.execute(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,
                        rating,culling_flag,favorite,scanned_at)
                     VALUES(?,?,?,?,?,?,0,?,?,?,0)",
                rusqlite::params![
                    i,
                    folder,
                    format!("IMG_{i:04}.jpg"),
                    i * 100,
                    if i % 10 == 0 { 1 } else { 0 }, // 10개마다 영상
                    taken,
                    i % 6,                          // 평점 0~5
                    if i % 7 == 0 { 2 } else { 0 }, // 7개마다 제외
                    i % 11 == 0,
                ],
            )?;
        }
        Ok(())
    })
    .unwrap();
    (dir, db)
}

/// 스크롤바가 준 순번으로 커서를 얻어 그 자리부터 읽는다.
/// 전부 읽은 목록의 같은 자리와 일치해야 한다 — 어긋나면 손잡이가 딴 데로 간다.
mod core;
mod facets_map;
