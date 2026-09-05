use super::*;
use std::sync::atomic::AtomicBool;

#[test]
#[ignore = "실제 DB가 있어야 한다 — ACUT_BENCH_DB 로 지정"]
fn offline_fill_on_a_real_library() {
    let Ok(src) = std::env::var("ACUT_BENCH_DB") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("bench.db");
    std::fs::copy(&src, &copy).expect("DB 사본을 만들지 못했습니다");
    let db = Db::open(&copy).unwrap();

    // 통계는 위치 사이드바를 열 때마다 돈다 — 여러 번 재서 가운데값을 본다
    let mut cold = vec![];
    let mut before = Stats::default();
    for _ in 0..5 {
        let t = std::time::Instant::now();
        before = stats(&db).unwrap();
        cold.push(t.elapsed().as_millis());
    }
    cold.sort();
    let stats_ms = cold[2];

    let t1 = std::time::Instant::now();
    let p = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let fill_ms = t1.elapsed().as_millis();
    let mut warm = vec![];
    let mut after = Stats::default();
    for _ in 0..5 {
        let t = std::time::Instant::now();
        after = stats(&db).unwrap();
        warm.push(t.elapsed().as_millis());
    }
    warm.sort();
    println!(
        "통계 — 채우기 전 {stats_ms}ms · 채운 뒤 {}ms (다섯 번의 가운데값)",
        warm[2]
    );

    // 첫 화면이 기다리는 질의들 — 어디가 오래 걸리는지 갈라 본다
    {
        use crate::db::query::{Filter, GroupBy};
        let f = Filter::default();
        let take = |name: &str, ms: Vec<u128>| {
            let mut ms = ms;
            ms.sort();
            println!("첫 화면 · {name} {}ms", ms[ms.len() / 2]);
        };
        let mut a = vec![];
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let _ = crate::db::query::page(&db, &f, None, 200, GroupBy::None).unwrap();
            a.push(t.elapsed().as_millis());
        }
        take("사진 첫 쪽 200장", a);
        let mut b = vec![];
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let _ = crate::db::query::summary(&db, &f).unwrap();
            b.push(t.elapsed().as_millis());
        }
        take("요약", b);
    }

    // 지도 칸 질의는 지도를 움직일 때마다 돈다 — 지명을 얹어 느려졌는지 직접 잰다.
    // 지명이 없던 시절의 질의를 나란히 돌려 같은 조건에서 견준다.
    let old_sql = "SELECT AVG(fi.gps_lat), AVG(fi.gps_lon), COUNT(*), MAX(fi.id)
                         FROM files fi WHERE fi.trashed_at IS NULL
                           AND fi.gps_lat IS NOT NULL AND fi.gps_lon IS NOT NULL
                           AND NOT (fi.gps_lat = 0.0 AND fi.gps_lon = 0.0)
                        GROUP BY CAST((fi.gps_lat + 90.0) / 0.1 AS INTEGER),
                                 CAST((fi.gps_lon + 180.0) / 0.1 AS INTEGER)
                        ORDER BY 3 DESC LIMIT 4000";
    let mut old_ms = vec![];
    let mut new_ms = vec![];
    let mut cells = vec![];
    for _ in 0..5 {
        let t = std::time::Instant::now();
        let n: usize = db
            .read(|c| {
                let mut st = c.prepare(old_sql)?;
                let it = st.query_map([], |r| r.get::<_, i64>(2))?;
                it.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .len();
        old_ms.push(t.elapsed().as_millis());
        let t = std::time::Instant::now();
        cells =
            crate::db::query::map_cells(&db, &crate::db::query::Filter::default(), 0.1).unwrap();
        new_ms.push(t.elapsed().as_millis());
        assert_eq!(n, cells.len(), "칸 수가 달라지면 견줄 수 없다");
    }
    old_ms.sort();
    new_ms.sort();
    let named = cells.iter().filter(|c| c.place.is_some()).count();
    println!(
            "지도 칸 — 지명 없이 {}ms · 지명 얹어 {}ms (다섯 번의 가운데값) · 칸 {} 개 · 이름 붙은 칸 {named} 개",
            old_ms[2], new_ms[2], cells.len()
        );
    if let Some(c) = cells.first() {
        println!(
            "가장 큰 칸: {:?} · {} 장 · 섞인 곳 {}",
            c.place, c.n, c.places
        );
    }

    println!(
        "통계 {stats_ms}ms · 오프라인 채우기 {fill_ms}ms\n\
             자리 {} 곳 · 사진 {} 장 · 못 정함 {} 곳\n\
             이름 붙은 사진 {} → {} (좌표 있는 사진 {})\n\
             남은 자리 {} → {} (서버만 가능 {})",
        p.done,
        p.files,
        p.empty,
        before.named,
        after.named,
        after.with_gps,
        before.cells_left,
        after.cells_left,
        after.network_cells_left,
    );
}
