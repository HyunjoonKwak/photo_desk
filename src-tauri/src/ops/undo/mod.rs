//! 되돌리기 — 작업 묶음 하나를 통째로 물린다.
//!
//! 저널에 남긴 (from, to)를 거꾸로 밟는다. 순서도 거꾸로다 — 같은 배치 안에서
//! 이름이 부딪혀 번호가 붙은 경우, 나중 것부터 물려야 원래 이름으로 돌아간다.
//!
//! 되돌린 배치는 `undone_at`이 찍힌다. 두 번 되돌리지 않기 위해서다.

use crate::db::conn::{Db, Result};
use crate::ops::trash::{move_with_sidecars, Outcome};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Batch {
    pub id: i64,
    pub kind: String,
    pub label: Option<String>,
    pub item_count: i64,
    pub created_at: i64,
    pub undone_at: Option<i64>,
}

/// 최근 작업 묶음들. 되돌릴 수 있는 것이 위에 온다.
///
/// 물릴 게 없어진 묶음은 먼저 닫는다 — 휴지통 화면에서 이미 되돌린 «휴지통으로»,
/// 다시 휴지통에 간 «되돌리기». 열어 두면 상태바 단추가 그걸 가리킨 채 남는다 (실측 2026-08-30)
pub fn recent(db: &Db, limit: usize) -> Result<Vec<Batch>> {
    let limit = limit.clamp(1, 200);
    // 닫히지 못한 묶음(도중에 멈춘 합치기 등) — 저널이 있으면 그 수로 닫아 되돌릴 수 있게
    const STALE_OPEN: &str = "item_count = 0 AND undone_at IS NULL
        AND EXISTS (SELECT 1 FROM journal j WHERE j.batch_id = batches.id AND j.ok = 1)
        AND created_at < strftime('%s','now') - 60";
    const TRASH_UNDONE_ELSEWHERE: &str = "undone_at IS NULL AND kind = 'trash' AND item_count > 0
        AND NOT EXISTS (SELECT 1 FROM journal j JOIN files f ON f.id = j.file_id
                        WHERE j.batch_id = batches.id AND j.ok = 1 AND f.trashed_at IS NOT NULL)";
    const RESTORE_UNDONE_ELSEWHERE: &str =
        "undone_at IS NULL AND kind = 'restore' AND item_count > 0
        AND NOT EXISTS (SELECT 1 FROM journal j JOIN files f ON f.id = j.file_id
                        WHERE j.batch_id = batches.id AND j.ok = 1 AND f.trashed_at IS NULL)";
    // 상태바가 갱신될 때마다 부르는 함수다. 고칠 묶음이 없으면 쓰기 연결을 잡지 않는다 —
    // 스캔·정리가 쓰기 뮤텍스를 쥔 동안 상태바가 그 뒤에 줄을 서면 안 된다 (2차 리뷰 M-10)
    let dirty: bool = db.read(|c| {
        c.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM batches WHERE {STALE_OPEN})
                     OR EXISTS (SELECT 1 FROM batches WHERE {TRASH_UNDONE_ELSEWHERE})
                     OR EXISTS (SELECT 1 FROM batches WHERE {RESTORE_UNDONE_ELSEWHERE})"
            ),
            [],
            |r| r.get(0),
        )
    })?;
    if dirty {
        db.write(|c| {
            c.execute(
                &format!(
                    "UPDATE batches SET item_count = (SELECT COUNT(*) FROM journal j WHERE j.batch_id = batches.id AND j.ok = 1)
                     WHERE {STALE_OPEN}"
                ),
                [],
            )?;
            c.execute(
                &format!("UPDATE batches SET undone_at = strftime('%s','now') WHERE {TRASH_UNDONE_ELSEWHERE}"),
                [],
            )?;
            c.execute(
                &format!("UPDATE batches SET undone_at = strftime('%s','now') WHERE {RESTORE_UNDONE_ELSEWHERE}"),
                [],
            )
        })?;
    }
    db.read(|c| {
        let mut st = c.prepare(
            "SELECT id, kind, label, item_count, created_at, undone_at
             FROM batches WHERE item_count > 0
               AND NOT EXISTS (SELECT 1 FROM folder_audit_children p
                               WHERE p.child_batch_id = batches.id)
             ORDER BY id DESC LIMIT ?1",
        )?;
        let it = st.query_map([limit as i64], |r| {
            Ok(Batch {
                id: r.get(0)?,
                kind: r.get(1)?,
                label: r.get(2)?,
                item_count: r.get(3)?,
                created_at: r.get(4)?,
                undone_at: r.get(5)?,
            })
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })
}

struct Row {
    file_id: i64,
    /// 원래 있던 볼륨 — 되돌리면 여기로 간다
    from_vol: String,
    from_path: String,
    /// 지금 있는 볼륨 — 볼륨을 넘어간 이동이면 `from_vol`과 다르다
    to_vol: String,
    to_path: String,
    /// 옮긴 직후의 크기·mtime. 없으면(옛 저널) 대조하지 않는다
    to_size: Option<i64>,
    to_mtime: Option<i64>,
}

/// 옮긴 뒤 그 자리의 파일이 바뀌었나. 저널에 기록이 없거나 파일을 읽지 못하면
/// «바뀌지 않았다»로 본다 — 없는 파일은 이어지는 이동이 제 오류로 알린다.
fn changed_since_recorded(path: &std::path::Path, row: &Row) -> bool {
    let (Some(size), Some(mtime)) = (row.to_size, row.to_mtime) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    i64::try_from(meta.len()).ok() != Some(size)
        || filetime::FileTime::from_last_modification_time(&meta).unix_seconds() != mtime
}

/// 배치 하나를 되돌린다.
pub fn undo(db: &Db, batch_id: i64) -> Result<Outcome> {
    let found: Option<(String, Option<i64>)> = db.read(|c| {
        use rusqlite::OptionalExtension;
        c.query_row(
            "SELECT kind, undone_at FROM batches WHERE id = ?1",
            [batch_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    })?;
    let Some((kind, undone)) = found else {
        return Ok(Outcome {
            batch_id,
            first_error: Some("없는 작업입니다. 목록을 다시 읽으세요".into()),
            ..Default::default()
        });
    };
    if undone.is_some() {
        // «휴지통으로» 묶음의 사진을 휴지통 화면에서 영구히 비우면 `recent()` 가 그 묶음도
        // 닫는다. 그건 되돌린 게 아니라 지운 것이다 — 남은 행이 없으면 그렇게 말한다.
        let survivors: i64 = db.read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM journal j JOIN files f ON f.id = j.file_id
                 WHERE j.batch_id = ?1 AND j.ok = 1",
                [batch_id],
                |r| r.get(0),
            )
        })?;
        let message = if kind == "trash" && survivors == 0 {
            "영구히 지운 사진은 되돌릴 수 없습니다"
        } else {
            "이미 되돌린 작업입니다"
        };
        return Ok(Outcome {
            batch_id,
            first_error: Some(message.into()),
            ..Default::default()
        });
    }
    // 영구히 지운 것은 되돌릴 수 없다 — 파일이 디스크에 없다. 되돌리기 후보에도 안 오르지만
    // (화면이 거른다) 명령으로 와도 거절한다
    if kind == "delete" {
        return Ok(Outcome {
            batch_id,
            first_error: Some("영구히 지운 사진은 되돌릴 수 없습니다".into()),
            ..Default::default()
        });
    }

    if kind == "capture_date" {
        return crate::ops::capture_date::undo(db, batch_id);
    }
    if kind == "copy" || kind == "publish" {
        return crate::ops::transfer::undo_copy(db, batch_id);
    }
    if kind == "folder_audit" {
        return crate::ops::p1::undo_folder_audit(db, batch_id);
    }
    if matches!(
        kind.as_str(),
        "folder_create" | "folder_rename" | "folder_move" | "folder_copy" | "folder_trash"
    ) {
        return crate::ops::folder::undo(db, batch_id);
    }

    // 가져오기는 되돌릴 곳이 없다. 원본은 카드에 그대로 있고, 되돌린다는 건
    // 「들여온 벌을 무른다」는 뜻이다. 그렇다고 지워 버리면 그것대로 되돌릴
    // 수 없으니 휴지통으로 보낸다.
    if kind == "import" {
        let ids: Vec<i64> = db.read(|c| {
            let mut st = c.prepare(
                "SELECT file_id FROM journal WHERE batch_id = ?1 AND ok = 1 AND file_id IS NOT NULL",
            )?;
            let it = st.query_map([batch_id], |r| r.get(0))?;
            it.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        let out = crate::ops::trash::to_trash(db, &ids, "가져오기 되돌리기")?;
        // 하나도 못 옮겼으면(카드·디스크가 빠짐) 배치를 열어 둔다 — 아래 일반 갈래와 같다
        if out.moved > 0 || ids.is_empty() {
            mark_undone(db, batch_id)?;
        }
        return Ok(Outcome { batch_id, ..out });
    }

    // 휴지통은 전용 경로가 있다 — trashed_at·trash_path를 함께 되돌려야 한다
    if kind == "trash" {
        let ids: Vec<i64> = db.read(|c| {
            let mut st = c.prepare(
                "SELECT j.file_id FROM journal j JOIN files f ON f.id = j.file_id
                 WHERE j.batch_id = ?1 AND j.ok = 1 AND f.trashed_at IS NOT NULL",
            )?;
            let it = st.query_map([batch_id], |r| r.get(0))?;
            it.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        // 휴지통 화면에서 이미 되돌렸으면 할 일이 없다 — 배치를 닫고 그렇게 말한다.
        // (열어 두면 «되돌리기» 단추가 눌러도 아무 일 없이 남는다 — 실측 2026-08-30)
        if ids.is_empty() {
            mark_undone(db, batch_id)?;
            return Ok(Outcome {
                batch_id,
                first_error: Some("이미 휴지통에서 되돌린 사진입니다".into()),
                ..Default::default()
            });
        }
        let out = crate::ops::trash::restore(db, &ids)?;
        if out.moved > 0 {
            mark_undone(db, batch_id)?;
        }
        return Ok(Outcome { batch_id, ..out });
    }

    // 휴지통에서 되돌린 것을 물린다 = 다시 휴지통으로. 그새 다른 길로 휴지통에 갔으면 할 일이 없다
    if kind == "restore" {
        let ids: Vec<i64> = db.read(|c| {
            let mut st = c.prepare(
                "SELECT j.file_id FROM journal j JOIN files f ON f.id = j.file_id
                 WHERE j.batch_id = ?1 AND j.ok = 1 AND f.trashed_at IS NULL",
            )?;
            let it = st.query_map([batch_id], |r| r.get(0))?;
            it.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        if ids.is_empty() {
            mark_undone(db, batch_id)?;
            return Ok(Outcome {
                batch_id,
                first_error: Some("이미 휴지통에 있는 사진입니다".into()),
                ..Default::default()
            });
        }
        let out = crate::ops::trash::to_trash(db, &ids, "되돌리기 취소 — 다시 휴지통으로")?;
        if out.moved > 0 {
            mark_undone(db, batch_id)?;
        }
        return Ok(Outcome { batch_id, ..out });
    }

    let rows: Vec<Row> = db.read(|c| {
        // 나중 것부터 — 같은 배치에서 이름이 밀린 경우를 제자리로 돌린다
        let mut st = c.prepare(
            "SELECT file_id, from_vol, from_path, COALESCE(to_vol, from_vol), to_path, to_size, to_mtime
             FROM journal
             WHERE batch_id = ?1 AND ok = 1 AND file_id IS NOT NULL AND to_path IS NOT NULL
             ORDER BY id DESC",
        )?;
        let it = st.query_map([batch_id], |r| {
            Ok(Row {
                file_id: r.get(0)?,
                from_vol: r.get(1)?,
                from_path: r.get(2)?,
                to_vol: r.get(3)?,
                to_path: r.get(4)?,
                to_size: r.get(5)?,
                to_mtime: r.get(6)?,
            })
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;

    let mut out = Outcome {
        batch_id,
        ..Default::default()
    };
    // 볼륨마다 마운트는 한 번만 찾는다
    let mut mounts: std::collections::HashMap<&str, Option<std::path::PathBuf>> =
        std::collections::HashMap::new();
    for row in &rows {
        let now_mount = mount_cached(&mut mounts, &row.to_vol);
        let back_mount = mount_cached(&mut mounts, &row.from_vol);
        let (Some(now_mount), Some(back_mount)) = (now_mount, back_mount) else {
            out.failed += 1;
            out.failed_ids.push(row.file_id);
            out.first_error
                .get_or_insert("디스크가 연결되어 있지 않습니다".into());
            continue;
        };
        // 지금 있는 곳. 옮긴 뒤 외부에서 같은 이름으로 교체된 파일을 원래 자리로 가져가
        // 원래 행의 평점·태그를 붙이면 안 된다 — 저널의 크기·mtime 과 다르면 그 항목만
        // 거절한다 (2차 리뷰 M-3)
        let from = now_mount.join(&row.to_path);
        if changed_since_recorded(&from, row) {
            out.failed += 1;
            out.failed_ids.push(row.file_id);
            out.first_error
                .get_or_insert("작업 뒤 파일 내용이 바뀌어 되돌리지 않았습니다".into());
            continue;
        }
        // 원래 자리에 그새 다른 파일이 생겼을 수 있다 — 덮어쓰지 않고 옆에 놓는다
        // (리뷰: rename은 있는 파일을 소리 없이 바꿔치기한다)
        let to = crate::ops::trash::free_path(back_mount.join(&row.from_path));
        let to_rel = to
            .strip_prefix(&back_mount)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| row.from_path.clone());
        match move_with_sidecars(&from, &to) {
            Ok(()) => match repoint(db, batch_id, row.file_id, &row.from_vol, &to_rel) {
                Ok(()) => out.moved += 1,
                Err(error) => {
                    let rollback = move_with_sidecars(&to, &from);
                    out.failed += 1;
                    out.failed_ids.push(row.file_id);
                    out.first_error.get_or_insert_with(|| match rollback {
                        Ok(()) => error.to_string(),
                        Err(rollback) => {
                            format!("DB 복원 실패: {error}; 파일 원위치 복구도 실패: {rollback}")
                        }
                    });
                }
            },
            Err(e) => {
                out.failed += 1;
                out.failed_ids.push(row.file_id);
                out.first_error.get_or_insert(e.to_string());
            }
        }
    }
    // 하나도 못 돌렸으면(디스크가 빠짐) 배치를 열어 둔다 — 꽂고 다시 시도할 수 있게
    let remaining: i64 = db.read(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM journal WHERE batch_id=?1 AND ok=1 AND file_id IS NOT NULL AND to_path IS NOT NULL",
            [batch_id],
            |r| r.get(0),
        )
    })?;
    if remaining == 0 {
        mark_undone(db, batch_id)?;
    }
    Ok(out)
}

fn mount_cached<'a>(
    m: &mut std::collections::HashMap<&'a str, Option<std::path::PathBuf>>,
    volume_uuid: &'a str,
) -> Option<std::path::PathBuf> {
    m.entry(volume_uuid)
        .or_insert_with(|| crate::db::volumes::find_mount(volume_uuid))
        .clone()
}

/// 파일 행이 원래 폴더를 가리키게 되돌린다. 폴더 행이 사라졌으면 되살린다.
fn repoint(db: &Db, batch_id: i64, file_id: i64, volume_uuid: &str, vol_rel: &str) -> Result<()> {
    let (dir, name) = match vol_rel.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), vol_rel.to_string()),
    };
    // 이 볼륨에서 그 경로를 품는 라이브러리를 찾는다 — 구역(area)도 그 라이브러리의 것.
    // 상수 1(내사진)로 박으면 작업대로 돌아온 폴더가 정착 구역으로 잡혀 고르기가 건너뛴다
    let lib: Option<(i64, i32)> = db.read(|c| {
        use rusqlite::OptionalExtension;
        c.query_row(
            "SELECT id, area FROM libraries WHERE volume_uuid = ?1
               AND (rel_path = '' OR ?2 = rel_path OR substr(?2, 1, length(rel_path) + 1) = rel_path || '/')
             ORDER BY length(rel_path) DESC LIMIT 1",
            rusqlite::params![volume_uuid, dir],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    })?;
    let library_id = lib.map(|l| l.0);
    let area = lib.map(|l| l.1).unwrap_or(0);
    let folder_name = dir.rsplit('/').next().unwrap_or(&dir).to_string();
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO folders(volume_uuid,library_id,rel_path,name,area,scanned_at)
             VALUES(?1,?2,?3,?4,?5,strftime('%s','now'))
             ON CONFLICT(volume_uuid,rel_path) DO UPDATE SET library_id=COALESCE(excluded.library_id, library_id)",
            rusqlite::params![volume_uuid, library_id, dir, folder_name, area],
        )?;
        tx.execute(
            "UPDATE files SET name = ?2,
                    folder_id = (SELECT id FROM folders WHERE volume_uuid=?3 AND rel_path=?4)
             WHERE id = ?1",
            rusqlite::params![file_id, name, volume_uuid, dir],
        )?;
        tx.execute(
            "UPDATE journal SET ok=0 WHERE batch_id=?1 AND file_id=?2 AND ok=1",
            rusqlite::params![batch_id,file_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

pub(crate) fn mark_undone(db: &Db, batch_id: i64) -> Result<()> {
    db.write(|c| {
        c.execute(
            "UPDATE batches SET undone_at = strftime('%s','now') WHERE id = ?1",
            [batch_id],
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests;
