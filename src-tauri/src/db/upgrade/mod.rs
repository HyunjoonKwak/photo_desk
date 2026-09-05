//! 기존 DB를 새 스키마에 맞춘다.
//!
//! `schema.sql`은 `CREATE TABLE IF NOT EXISTS`라서 **이미 있는 테이블에 컬럼을
//! 더하지는 못한다.** 그런 변경만 여기서 처리한다. 새로 만든 DB에서는 전부
//! 아무 일도 하지 않는 no-op이 되어야 한다.

use rusqlite::Connection;

pub fn run(c: &Connection) -> rusqlite::Result<()> {
    add_library_id(c)?;
    backfill_libraries(c)?;
    add_trash_columns(c)?;
    add_faces_at(c)?;
    add_image_hash(c)?;
    add_phash(c)?;
    add_live_count_index(c)?;
    add_done_at(c)?;
    add_geo_levels(c)?;
    add_nas_pulls(c)?;
    add_gallery_transition_p0(c)?;
    add_gallery_transition_p1(c)?;
    add_release_091_integrity(c)?;
    add_journal_file_stat(c)?;
    add_folder_journal_stat(c)?;
    rename_old_labels(c)?;
    migrate_taken_at_to_utc(c)?;
    Ok(())
}

/// 0.9.1 무결성 보강. 0.9.0 저널은 해시가 없으므로 NULL로 남겨 두고 undo에서
/// 보수적으로 거절한다. 새 작업만 완전한 before/after 및 copy manifest를 가진다.
fn add_release_091_integrity(c: &Connection) -> rusqlite::Result<()> {
    for (column, ddl) in [
        (
            "before_sha256",
            "ALTER TABLE capture_date_journal ADD COLUMN before_sha256 TEXT",
        ),
        (
            "after_sha256",
            "ALTER TABLE capture_date_journal ADD COLUMN after_sha256 TEXT",
        ),
        (
            "undone_at",
            "ALTER TABLE capture_date_journal ADD COLUMN undone_at INTEGER",
        ),
    ] {
        if !has_column(c, "capture_date_journal", column)? {
            c.execute_batch(ddl)?;
        }
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS copy_manifest (
            batch_id INTEGER NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
            file_id INTEGER NOT NULL,
            seq INTEGER NOT NULL,
            to_vol TEXT NOT NULL,
            to_path TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            is_main INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (batch_id,file_id,seq),
            UNIQUE (batch_id,to_vol,to_path)
         );",
    )
}

/// 일반 되돌리기(move·rename·trash·restore)의 동일성 대조용 — 옮긴 직후 목적지의
/// 크기·mtime. 이전 저널은 NULL 로 남아 대조 없이 되돌린다 (2차 리뷰 M-3).
fn add_journal_file_stat(c: &Connection) -> rusqlite::Result<()> {
    for (column, ddl) in [
        ("to_size", "ALTER TABLE journal ADD COLUMN to_size INTEGER"),
        (
            "to_mtime",
            "ALTER TABLE journal ADD COLUMN to_mtime INTEGER",
        ),
    ] {
        if !has_column(c, "journal", column)? {
            c.execute_batch(ddl)?;
        }
    }
    Ok(())
}

/// 같은 볼륨 폴더 이름변경·이동·휴지통은 내용 해시 대신 이름·크기·mtime 다이제스트로
/// undo 를 대조한다. 0.9.1 저널은 NULL 로 남아 내용 해시로 대조한다 (2차 리뷰 M-11).
fn add_folder_journal_stat(c: &Connection) -> rusqlite::Result<()> {
    if !has_column(c, "folder_journal", "stat_sha256")? {
        c.execute_batch("ALTER TABLE folder_journal ADD COLUMN stat_sha256 TEXT")?;
    }
    Ok(())
}

/// Gallery→Desk P1 폴더명 감사의 부모→자식 배치 연결. 신규·기존 DB 모두 멱등이다.
fn add_gallery_transition_p1(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS folder_audit_children (
            parent_batch_id INTEGER NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
            child_batch_id INTEGER NOT NULL UNIQUE REFERENCES batches(id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            PRIMARY KEY (parent_batch_id, child_batch_id)
         );",
    )
}

/// Gallery→Desk P0 작업용 테이블. CREATE IF NOT EXISTS라 구버전·신규 DB 모두 멱등이다.
fn add_gallery_transition_p0(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS capture_date_journal (
            batch_id INTEGER NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            backup_vol TEXT, backup_path TEXT,
            old_atime_sec INTEGER NOT NULL, old_atime_nsec INTEGER NOT NULL,
            old_mtime_sec INTEGER NOT NULL, old_mtime_nsec INTEGER NOT NULL,
            old_taken_at INTEGER NOT NULL, old_source INTEGER NOT NULL,
            old_override INTEGER, new_taken_at INTEGER NOT NULL,
            write_scope TEXT NOT NULL,
            PRIMARY KEY (batch_id, file_id)
         );
         CREATE TABLE IF NOT EXISTS capture_date_overrides (
            file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            taken_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
         );
         CREATE TABLE IF NOT EXISTS publication_ledger (
            id INTEGER PRIMARY KEY,
            source_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL,
            source_sha256 TEXT NOT NULL,
            destination_library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
            destination_path TEXT NOT NULL,
            destination_sha256 TEXT NOT NULL,
            batch_id INTEGER NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            UNIQUE(source_sha256,destination_library_id,destination_path)
         );
         CREATE INDEX IF NOT EXISTS idx_publication_hash ON publication_ledger(source_sha256,destination_library_id);
         CREATE INDEX IF NOT EXISTS idx_publication_batch ON publication_ledger(batch_id);
         CREATE TABLE IF NOT EXISTS folder_journal (
            batch_id INTEGER PRIMARY KEY REFERENCES batches(id) ON DELETE CASCADE,
            op TEXT NOT NULL,
            source_library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
            source_path TEXT NOT NULL,
            destination_library_id INTEGER REFERENCES libraries(id) ON DELETE CASCADE,
            destination_path TEXT,
            file_count INTEGER NOT NULL DEFAULT 0,
            dir_count INTEGER NOT NULL DEFAULT 0,
            bytes INTEGER NOT NULL DEFAULT 0,
            manifest_sha256 TEXT NOT NULL,
            cross_volume INTEGER NOT NULL DEFAULT 0
         );",
    )
}

/// 초기 버전이 UTC처럼 저장했던 시간대 없는 EXIF/파일명 시각을 실제 Unix
/// 시각으로 한 번만 바꾼다. 파일명은 재파싱해 13자리 epoch 값은 이동하지 않는다.
fn migrate_taken_at_to_utc(c: &Connection) -> rusqlite::Result<()> {
    migrate_taken_at_to_utc_in_chunks(c, 5_000)
}

fn migrate_taken_at_to_utc_in_chunks(c: &Connection, chunk_size: usize) -> rusqlite::Result<()> {
    const KEY: &str = "internal.taken_at_utc_v1";
    let done: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings WHERE key = ?1)",
        [KEY],
        |r| r.get(0),
    )?;
    if done {
        return Ok(());
    }

    assert!(chunk_size > 0, "날짜 마이그레이션 묶음 크기는 0일 수 없다");
    let tx = c.unchecked_transaction()?;
    let mut last_id = i64::MIN;
    let mut first_chunk = true;
    loop {
        let rows: Vec<(i64, String, i32, i32, i64, String)> = {
            let comparison = if first_chunk { ">=" } else { ">" };
            let sql = format!(
                "SELECT fi.id, fi.name, fi.kind, fi.taken_at_source, fi.taken_at, fo.rel_path
                 FROM files fi JOIN folders fo ON fo.id = fi.folder_id
                 WHERE fi.id {comparison} ?1
                   AND ((fi.taken_at_source = 0 AND fi.kind != 1)
                        OR fi.taken_at_source = 1)
                 ORDER BY fi.id
                 LIMIT ?2"
            );
            let mut st = tx.prepare(&sql)?;
            let mapped = st.query_map(rusqlite::params![last_id, chunk_size as i64], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let Some(next_last_id) = rows.last().map(|row| row.0) else {
            break;
        };
        let mut update = tx.prepare("UPDATE files SET taken_at = ?2 WHERE id = ?1")?;
        for (id, name, kind, source, old, folder) in rows {
            let migrated = match source {
                0 if kind != 1 => crate::media::taken_at::floating_civil_to_unix(old),
                1 => crate::media::taken_at::from_filename(&name)
                    .or_else(|| {
                        folder
                            .rsplit('/')
                            .next()
                            .and_then(crate::media::taken_at::from_filename)
                    })
                    .unwrap_or(old),
                _ => old,
            };
            if migrated != old {
                update.execute(rusqlite::params![id, migrated])?;
            }
        }
        last_id = next_last_id;
        first_chunk = false;
    }
    tx.execute("INSERT INTO settings(key,value) VALUES(?1,'1')", [KEY])?;
    tx.commit()
}

/// 되돌리기 목록의 옛 낱말 — «치우기»를 없애고 «휴지통으로»로 부르기로 했다(2026-08-29).
/// 이미 저장된 묶음 이름도 같은 낱말이어야 단추가 «되돌리기: 제외한 사진 치우기»로 안 뜬다
fn rename_old_labels(c: &Connection) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE batches SET label = replace(label, '치우기', '휴지통으로') WHERE label LIKE '%치우기%'",
        [],
    )?;
    Ok(())
}

/// 휴지통 표시용 컬럼. 파일 행을 지우지 않고 표시만 하는 이유는
/// 되돌릴 때 평점·판정이 살아남아야 하기 때문이다.
fn add_trash_columns(c: &Connection) -> rusqlite::Result<()> {
    for (col, ddl) in [
        ("trashed_at", "ALTER TABLE files ADD COLUMN trashed_at INTEGER"),
        ("trash_path", "ALTER TABLE files ADD COLUMN trash_path TEXT"),
        (
            "trash_batch",
            "ALTER TABLE files ADD COLUMN trash_batch INTEGER REFERENCES batches(id) ON DELETE SET NULL",
        ),
    ] {
        if !has_column(c, "files", col)? {
            c.execute_batch(ddl)?;
        }
    }
    c.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_trashed ON files(trashed_at) WHERE trashed_at IS NOT NULL;",
    )
}

/// 얼굴을 찾아 본 시각 — 얼굴이 없어도 남아 다음에 다시 보지 않는다 (4단계)
/// 메타데이터만 다른 사본을 찾는 «그림 해시»(2026-08-30) — 촬영일시 EXIF 를 나중에 써 넣은
/// 사본은 바이트가 달라 완전 중복에서 빠졌다 (실측: 하와이 1,081장)
fn add_image_hash(c: &Connection) -> rusqlite::Result<()> {
    if !has_column(c, "files", "image_hash")? {
        c.execute_batch("ALTER TABLE files ADD COLUMN image_hash TEXT")?;
    }
    c.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_image_hash ON files(image_hash) WHERE image_hash IS NOT NULL;",
    )
}

/// 크기만 줄인 사본을 찾는 지각 해시(2026-09-01). 64비트를 i64 로 담는다 —
/// SQLite 정수가 부호 있는 64비트라 u64 를 그대로는 못 넣는다. 읽을 때 되돌린다.
/// 색인은 두지 않는다 — 같은 값 찾기가 아니라 전량을 메모리로 올려 견주기 때문이다.
fn add_phash(c: &Connection) -> rusqlite::Result<()> {
    if !has_column(c, "files", "phash")? {
        c.execute_batch("ALTER TABLE files ADD COLUMN phash INTEGER")?;
    }
    // 버전+16×16 밝기+8×8 색차 — 해시가 이은 짝이 정말 같은 그림인지 견준다
    if !has_column(c, "files", "psig")? {
        c.execute_batch("ALTER TABLE files ADD COLUMN psig BLOB")?;
    }
    Ok(())
}

/// 라이브러리별 «살아 있는 사진 수»를 세는 부분 인덱스(2026-09-01).
///
/// `idx_files_folder` 는 `trashed_at` 을 담지 않아, 세려면 14.6만 행을 하나씩 다시
/// 읽어야 했다. 그 한 질의가 **3.16초** — 첫 화면 2.5초의 거의 전부였다.
/// 이 인덱스로 0.005초가 된다. 구버전 DB 에도 만들어 준다.
fn add_live_count_index(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_folder_live ON files(folder_id) WHERE trashed_at IS NULL;",
    )
}

/// «처리됨 보기»(2026-08-31) — 확정한 무리를 최근 순으로 다시 보고 무리 단위로 취소한다
fn add_done_at(c: &Connection) -> rusqlite::Result<()> {
    if !has_column(c, "groups", "done_at")? {
        c.execute_batch("ALTER TABLE groups ADD COLUMN done_at INTEGER")?;
    }
    Ok(())
}

/// 지명 3단계(2026-09-01) — 국가·시도·시군구와 격자 캐시. 좌표만 보이던 위치 갈래를
/// 사람이 읽는 이름으로 묶기 위한 것. 값은 «지명 채우기»가 나중에 채운다.
fn add_geo_levels(c: &Connection) -> rusqlite::Result<()> {
    for col in ["geo_country", "geo_admin1", "geo_admin2"] {
        if !has_column(c, "files", col)? {
            c.execute_batch(&format!("ALTER TABLE files ADD COLUMN {col} TEXT"))?;
        }
    }
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS places (
            cell TEXT PRIMARY KEY, country TEXT, admin1 TEXT, admin2 TEXT, name TEXT,
            status TEXT NOT NULL DEFAULT 'ok', at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_files_geo ON files(geo_country, geo_admin1, geo_admin2);",
    )?;
    if !has_column(c, "places", "status")? {
        c.execute_batch("ALTER TABLE places ADD COLUMN status TEXT NOT NULL DEFAULT 'ok'")?;
    }
    // 출처·정밀도 (2026-09-01) — 오프라인 지명이 들어오면 «어디서 온 값인지»로
    // 덮어쓰기를 판단해야 한다. status 하나에 출처를 섞지 않는다
    for (col, decl) in [
        ("source", "TEXT NOT NULL DEFAULT 'legacy'"),
        ("precision", "TEXT"),
        ("distance_km", "REAL"),
        ("dataset_version", "TEXT"),
        ("provider", "TEXT"),
        ("resolved_at", "INTEGER"),
        // 온라인 조회 이력 (2026-09-01) — 값의 출처와 다른 축이다. 서버가 못
        // 찾았거나 얕게 답했을 때 값은 그대로 두고 «물어봤다»만 남겨야, 같은
        // 좌표를 같은 서버에 되풀이해 묻지 않는다
        ("online_outcome", "TEXT"),
        ("online_provider", "TEXT"),
        ("online_checked_at", "INTEGER"),
    ] {
        if !has_column(c, "places", col)? {
            c.execute_batch(&format!("ALTER TABLE places ADD COLUMN {col} {decl}"))?;
        }
    }
    // 옛 판의 «이름 없음»은 온라인이 그렇게 답한 것이다. 조회 이력 칸이 생기기
    // 전에 만들어졌으므로 여기서 채워 준다 — 비워 두면 «아직 아무한테도 안
    // 물어봤다»로 읽혀 대상에 다시 들어간다. 어느 서버였는지는 알 수 없으니
    // online_provider 는 비워 둔다: 서버를 설정하면 딱 한 번 다시 물어보고,
    // 그때 서버 이름이 기록돼 그다음부터는 조용해진다.
    c.execute_batch(
        "UPDATE places
            SET online_outcome = 'none',
                online_provider = provider,
                online_checked_at = COALESCE(resolved_at, at)
          WHERE status = 'none' AND online_outcome IS NULL;",
    )?;
    // 기존 캐시는 모두 온라인에서 온 것이다 — 오프라인 경로가 없던 시절의 값이다
    c.execute_batch(
        "UPDATE places SET source='nominatim', precision='remote',
                resolved_at=COALESCE(resolved_at, at)
          WHERE source='legacy' AND status='ok'
            AND country IS NOT NULL AND trim(country) <> '';
         UPDATE places SET source='nominatim', resolved_at=COALESCE(resolved_at, at)
          WHERE source='legacy' AND status='none';
         CREATE INDEX IF NOT EXISTS idx_places_status ON places(status, source);
         CREATE INDEX IF NOT EXISTS idx_places_online ON places(online_outcome, online_provider);",
    )?;
    // 첫 지명 구현은 «결과 없음»도 세 이름이 모두 NULL인 캐시 행으로 남겼다.
    // status를 단순 DEFAULT 'ok'로 더하면 그 행은 성공 캐시가 되어 다시 묻지도,
    // 파일을 완료시키지도 못한다. 이미 그 중간 빌드를 열어 status가 생긴 DB도
    // 복구해야 하므로 컬럼 추가 여부와 무관하게 매번 멱등으로 보정한다.
    //
    // **`status='ok'` 인 행만 고친다.** 이 보정은 앱을 열 때마다 도는데, 조건을
    // «이름이 비었으면»으로 잡으면 오프라인 판정이 남긴 `unresolved`(이름이 비어
    // 있는 것이 정상이다)까지 `none` 으로 바꿔 버린다. 그러면 그 자리는 다시
    // 물어볼 수 없는 곳으로 굳어 영영 이름을 얻지 못한다 (2026-09-01 외부 검토).
    c.execute_batch(
        "UPDATE places
            SET status = 'none'
          WHERE status = 'ok' AND (country IS NULL OR trim(country) = '');",
    )?;
    Ok(())
}

fn add_faces_at(c: &Connection) -> rusqlite::Result<()> {
    if !has_column(c, "files", "faces_at")? {
        c.execute_batch("ALTER TABLE files ADD COLUMN faces_at INTEGER")?;
    }
    c.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_files_faces_at ON files(faces_at) WHERE faces_at IS NULL;",
    )
}

/// NAS 1차 구역에서 내려받은 것의 원장 — 비울 때 «우리가 받은 것»만 고른다 (5단계)
fn add_nas_pulls(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS nas_pulls (
            rel_path  TEXT PRIMARY KEY,
            size      INTEGER NOT NULL,
            pulled_at INTEGER NOT NULL
        );",
    )
}

fn has_column(c: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut st = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        if r.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `folders.library_id`와 그 인덱스를 보장한다.
///
/// 인덱스를 `schema.sql`에 두면 안 된다. 스키마 배치는 이 함수보다 **먼저**
/// 도는데, 구버전 DB에는 그 시점에 컬럼이 없어 배치 전체가 실패한다.
fn add_library_id(c: &Connection) -> rusqlite::Result<()> {
    if !has_column(c, "folders", "library_id")? {
        c.execute_batch(
            "ALTER TABLE folders ADD COLUMN library_id INTEGER
                 REFERENCES libraries(id) ON DELETE CASCADE;",
        )?;
    }
    c.execute_batch("CREATE INDEX IF NOT EXISTS idx_folders_lib ON folders(library_id);")
}

/// 라이브러리 층이 생기기 전에 스캔한 폴더들을 라이브러리에 붙인다.
///
/// 그때는 볼륨 하나가 곧 라이브러리 하나였다. 그래서 **볼륨마다 폴더 경로의
/// 공통 앞부분**을 찾으면 그게 그 시절의 라이브러리 루트다.
/// 예: `MERGE/사진통합작업/연도별/…`가 전부라면 루트는 `MERGE/사진통합작업`.
fn backfill_libraries(c: &Connection) -> rusqlite::Result<()> {
    let orphan_volumes: Vec<String> = {
        let mut st =
            c.prepare("SELECT DISTINCT volume_uuid FROM folders WHERE library_id IS NULL")?;
        let it = st.query_map([], |r| r.get::<_, String>(0))?;
        it.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for vol in orphan_volumes {
        let paths: Vec<String> = {
            let mut st = c.prepare(
                "SELECT rel_path FROM folders WHERE volume_uuid = ?1 AND library_id IS NULL",
            )?;
            let it = st.query_map([&vol], |r| r.get::<_, String>(0))?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let root = common_dir_prefix(&paths);
        let name = if root.is_empty() {
            c.query_row("SELECT name FROM volumes WHERE uuid = ?1", [&vol], |r| {
                r.get::<_, String>(0)
            })
            .unwrap_or_else(|_| vol.clone())
        } else {
            root.rsplit('/').next().unwrap_or(&root).to_string()
        };

        c.execute(
            "INSERT INTO libraries(volume_uuid, rel_path, name)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(volume_uuid, rel_path) DO UPDATE SET name = excluded.name",
            rusqlite::params![vol, root, name],
        )?;
        let id: i64 = c.query_row(
            "SELECT id FROM libraries WHERE volume_uuid = ?1 AND rel_path = ?2",
            rusqlite::params![vol, root],
            |r| r.get(0),
        )?;
        c.execute(
            "UPDATE folders SET library_id = ?1 WHERE volume_uuid = ?2 AND library_id IS NULL",
            rusqlite::params![id, vol],
        )?;
    }
    Ok(())
}

/// 경로들의 공통 **디렉터리** 앞부분. 글자 단위가 아니라 `/` 단위로 자른다.
///
/// 글자 단위로 하면 `2003`과 `2004`에서 `200`이 나와 실재하지 않는 폴더가 된다.
///
/// 경로 하나가 다른 것들의 부모이면 그게 그대로 답이다. 스캐너는 **파일이 든
/// 폴더만** 기록하므로, 루트에 사진이 흩어져 있으면 루트도 목록에 들어 있다.
pub fn common_dir_prefix(paths: &[String]) -> String {
    let mut it = paths.iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut prefix: Vec<&str> = first.split('/').collect();

    for p in it {
        let parts: Vec<&str> = p.split('/').collect();
        let keep = prefix
            .iter()
            .zip(parts.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(keep);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.join("/")
}

#[cfg(test)]
mod tests;
