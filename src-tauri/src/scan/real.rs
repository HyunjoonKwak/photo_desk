use super::*;

/// `cargo test --release --lib scan::real -- --ignored --nocapture`
#[test]
#[ignore = "실제 라이브러리 전체를 스캔한다"]
fn scan_the_whole_library() {
    // 어느 라이브러리를 잴지는 ACUT_BENCH_ROOT로 준다. 없으면 옛 자리.
    let root_s = std::env::var("ACUT_BENCH_ROOT")
        .unwrap_or_else(|_| "/Volumes/MAIN SSD/MERGE/사진통합작업".into());
    let root = Path::new(&root_s);
    if !root.is_dir() {
        eprintln!("라이브러리가 없다 — 건너뜀");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("acut.db")).unwrap();

    let t0 = std::time::Instant::now();
    let last = std::sync::Mutex::new(std::time::Instant::now());
    let p = scan_test(&db, root, 1, |pr| {
        let mut l = last.lock().unwrap();
        if l.elapsed().as_secs() >= 2 {
            let done = pr.inserted + pr.updated + pr.skipped;
            eprintln!(
                "   {done:>7}/{} · {:.0}s",
                pr.found,
                t0.elapsed().as_secs_f64()
            );
            *l = std::time::Instant::now();
        }
    })
    .expect("스캔");
    let secs = t0.elapsed().as_secs_f64();

    println!("\n═══ 실제 라이브러리 스캔 ═══");
    println!("  발견   {:>7}", p.found);
    println!("  삽입   {:>7}", p.inserted);
    println!("  실패   {:>7}", p.failed);
    println!(
        "  소요   {secs:>7.1}초  ({:.0}장/초)",
        p.found as f64 / secs
    );

    // 쿼리 성능 — 스캔 직후 실제 데이터로
    let bench = |label: &str, sql: &str| {
        let t = std::time::Instant::now();
        let n: i64 = db.read(|c| c.query_row(sql, [], |r| r.get(0))).unwrap();
        println!(
            "  {label:<28} {:>7.1} ms  (n={n})",
            t.elapsed().as_secs_f64() * 1000.0
        );
    };
    println!("\n═══ 쿼리 ═══");
    bench("전체 개수", "SELECT COUNT(*) FROM files");
    bench(
        "최신 200장",
        "SELECT COUNT(*) FROM (SELECT id FROM files ORDER BY taken_at DESC LIMIT 200)",
    );
    bench("RAW만", "SELECT COUNT(*) FROM files WHERE kind=2");
    bench(
        "GPS 있는 것",
        "SELECT COUNT(*) FROM files WHERE gps_lat IS NOT NULL",
    );
    bench("카메라별", "SELECT COUNT(DISTINCT cam_model) FROM files");

    // 촬영일 출처 분포 — 폴백 체인이 실제로 어떻게 작동했는지
    println!("\n═══ 촬영일 출처 ═══");
    let rows: Vec<(i64, i64)> = db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT taken_at_source, COUNT(*) FROM files GROUP BY 1 ORDER BY 2 DESC",
            )?;
            let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            it.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap();
    for (src, n) in rows {
        let label = match src {
            0 => "EXIF",
            1 => "파일명",
            2 => "파일시각",
            _ => "불명",
        };
        println!("  {label:<10} {n:>7}");
    }
    println!();
    assert!(p.found > 1000, "실제 라이브러리를 찾아야 한다");
}

/// 스캔 + 썸네일 전체 파이프라인.
/// `cargo test --release --lib scan::real::full_pipeline -- --ignored --nocapture`
#[test]
#[ignore = "실제 라이브러리 전체 · 수 분 걸린다"]
fn full_pipeline() {
    // 어느 라이브러리를 잴지는 ACUT_BENCH_ROOT로 준다. 없으면 옛 자리.
    let root_s = std::env::var("ACUT_BENCH_ROOT")
        .unwrap_or_else(|_| "/Volumes/MAIN SSD/MERGE/사진통합작업".into());
    let root = Path::new(&root_s);
    if !root.is_dir() {
        eprintln!("라이브러리 없음 — 건너뜀");
        return;
    }
    // 캐시는 쓰기 가능한 임시 폴더에 만든다 (원본 볼륨을 건드리지 않는다)
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("acut.db")).unwrap();

    let t0 = std::time::Instant::now();
    let p = scan_test(&db, root, 1, |_| {}).expect("스캔");
    let scan_s = t0.elapsed().as_secs_f64();
    println!("\n═══ 1단계 스캔 ═══");
    println!(
        "  {}장 · {:.1}초 · {:.0}장/초",
        p.found,
        scan_s,
        p.found as f64 / scan_s
    );

    let vol = crate::db::volumes::describe(root).unwrap();
    let lib: i64 = db
        .read(|c| c.query_row("SELECT id FROM libraries", [], |r| r.get(0)))
        .unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t1 = std::time::Instant::now();
    let last = std::sync::Mutex::new(std::time::Instant::now());
    let tp = thumbs::generate(
        &db,
        lib,
        &vol.mount_path,
        &tmp.path().join("cache"),
        cancel,
        |pr| {
            let mut l = last.lock().unwrap();
            if l.elapsed().as_secs() >= 5 {
                eprintln!(
                    "   썸네일 {}/{} · {:.0}s",
                    pr.done,
                    pr.total,
                    t1.elapsed().as_secs_f64()
                );
                *l = std::time::Instant::now();
            }
        },
    )
    .expect("썸네일");
    let thumb_s = t1.elapsed().as_secs_f64();

    println!("\n═══ 2단계 썸네일 ═══");
    println!(
        "  대상 {}장 · 성공 {} · 실패 {}",
        tp.total,
        tp.done - tp.failed,
        tp.failed
    );
    println!(
        "  {:.1}초 · {:.0}장/초 · {:.1}ms/장",
        thumb_s,
        tp.total as f64 / thumb_s,
        thumb_s * 1000.0 / tp.total as f64
    );

    let (bytes, count) = crate::media::cache::cache_stats(&tmp.path().join("cache"));
    println!(
        "  캐시 {}개 · {:.0} MB (원본 대비 {:.1}%)",
        count,
        bytes as f64 / 1024.0 / 1024.0,
        bytes as f64 / 373.5 / 1024.0 / 1024.0 / 1024.0 * 100.0
    );
    println!("\n  전체 {:.1}초\n", scan_s + thumb_s);
}
