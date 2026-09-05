use super::super::apply::ApplyAll;
use rusqlite::{params, Transaction};
use serde::Serialize;

/// 폴더 묶음 하나를 처리한다 — `keep` 폴더의 파일은 남김, `drops` 폴더의 파일은 제외 표시.
/// 이 폴더들 안에서만 얽힌 완전 중복 무리는 확정으로 돌려 개별 비교에 다시 안 나오게.
pub fn apply_set(tx: &Transaction, keep: i64, drops: &[i64]) -> rusqlite::Result<ApplyAll> {
    apply_trees(tx, &[keep], drops)
}

/// 폴더 «나무» 둘 — 남길 쪽 폴더들(`keep`)과 제외할 쪽 폴더들(`drop`). 두 폴더 비교가 하위
/// 폴더까지 통째로 짝지을 때 쓴다. 제외는 **남길 쪽 나무 어딘가에 같은 내용이 지금 있는**
/// 파일에만 붙는다 — 남길 쪽에 없는 사진이 지워지는 일은 없다 (리뷰 C12).
///
/// 이미 «남김»(1)인 파일은 내리지 않는다 — 남김은 결정이다. 남김이 붙은 폴더는 비교 화면이
/// 애초에 제외 후보로 올리지 않는다(`kept_a`/`kept_b`). 지우고 싶으면 먼저 «표시 취소»
pub fn apply_trees(tx: &Transaction, keep: &[i64], drop: &[i64]) -> rusqlite::Result<ApplyAll> {
    if keep.is_empty() || drop.is_empty() || keep.iter().any(|k| drop.contains(k)) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let keep_list = keep
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let drop_list = drop
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let kept = tx.execute(
        &format!("UPDATE files SET culling_flag = 1 WHERE folder_id IN ({keep_list}) AND trashed_at IS NULL"),
        [],
    )?;
    let rejected = tx.execute(
        &format!(
            "UPDATE files SET culling_flag = 2
             WHERE folder_id IN ({drop_list}) AND trashed_at IS NULL AND culling_flag <> 1
               AND full_hash IS NOT NULL
               AND full_hash IN (SELECT k.full_hash FROM files k
                                 WHERE k.folder_id IN ({keep_list}) AND k.trashed_at IS NULL AND k.full_hash IS NOT NULL)"
        ),
        [],
    )?;
    let list = format!("{keep_list},{drop_list}");
    let groups = tx.execute(
        &format!(
            "UPDATE groups SET state = 1, done_at = strftime('%s','now') WHERE kind = 0 AND state = 0
               AND id IN (SELECT m.group_id FROM group_members m JOIN files f ON f.id = m.file_id
                          WHERE f.folder_id IN ({list}))
               AND NOT EXISTS (SELECT 1 FROM group_members m JOIN files f ON f.id = m.file_id
                               WHERE m.group_id = groups.id AND f.folder_id NOT IN ({list}))"
        ),
        params![],
    )?;
    Ok(ApplyAll {
        groups,
        kept,
        rejected,
        skipped: 0,
    })
}

/// 두 폴더 사이의 완전 중복 무리를 한꺼번에 — `keep` 폴더 것을 남기고 `drop` 폴더 것에 제외
/// 표시. 두 폴더 밖까지 얽힌 무리는 건너뛴다(그건 개별 비교에서). 개별 비교의 «이 폴더 쌍
/// 전부 이렇게» 단추가 쓴다.
pub fn apply_pair(
    tx: &Transaction,
    keep: i64,
    drop: i64,
    dry_run: bool,
) -> rusqlite::Result<ApplyAll> {
    if keep == drop {
        return Err(rusqlite::Error::InvalidQuery);
    }
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.todo; CREATE TEMP TABLE todo(id INTEGER PRIMARY KEY);",
    )?;
    tx.execute(
        "INSERT INTO temp.todo
         SELECT g.id FROM groups g
         WHERE g.kind = 0 AND g.state = 0
           AND EXISTS (SELECT 1 FROM group_members m JOIN files f ON f.id = m.file_id
                       WHERE m.group_id = g.id AND f.folder_id = ?1)
           AND EXISTS (SELECT 1 FROM group_members m JOIN files f ON f.id = m.file_id
                       WHERE m.group_id = g.id AND f.folder_id = ?2)
           AND NOT EXISTS (SELECT 1 FROM group_members m JOIN files f ON f.id = m.file_id
                           WHERE m.group_id = g.id AND f.folder_id NOT IN (?1, ?2))",
        params![keep, drop],
    )?;
    let groups =
        tx.query_row("SELECT COUNT(*) FROM temp.todo", [], |r| r.get::<_, i64>(0))? as usize;
    if dry_run {
        // 세기만 — 남길 폴더의 구성원이 남김, 나머지 중 아직 «남김»이 아닌 것이 제외
        let kept = tx.query_row(
            "SELECT COUNT(DISTINCT m.file_id) FROM group_members m JOIN files f ON f.id = m.file_id
             WHERE m.group_id IN (SELECT id FROM temp.todo) AND f.folder_id = ?1",
            [keep],
            |r| r.get::<_, i64>(0),
        )? as usize;
        let rejected = tx.query_row(
            "SELECT COUNT(DISTINCT m.file_id) FROM group_members m JOIN files f ON f.id = m.file_id
             WHERE m.group_id IN (SELECT id FROM temp.todo) AND f.folder_id <> ?1 AND f.culling_flag <> 1",
            [keep],
            |r| r.get::<_, i64>(0),
        )? as usize;
        tx.execute_batch("DROP TABLE temp.todo;")?;
        return Ok(ApplyAll {
            groups,
            kept,
            rejected,
            skipped: 0,
        });
    }
    tx.execute(
        "UPDATE group_members SET is_best = CASE WHEN file_id IN (SELECT id FROM files WHERE folder_id = ?1) THEN 1 ELSE 0 END
         WHERE group_id IN (SELECT id FROM temp.todo)",
        [keep],
    )?;
    let kept = tx.execute(
        "UPDATE files SET culling_flag = 1 WHERE id IN (
           SELECT file_id FROM group_members WHERE group_id IN (SELECT id FROM temp.todo) AND is_best = 1)",
        [],
    )?;
    // 이미 «남김»인 파일은 내리지 않는다 — 다른 갈래와 같은 규칙 (리뷰 C11)
    let rejected = tx.execute(
        "UPDATE files SET culling_flag = 2 WHERE culling_flag <> 1 AND id IN (
           SELECT file_id FROM group_members WHERE group_id IN (SELECT id FROM temp.todo) AND is_best = 0)",
        [],
    )?;
    tx.execute("UPDATE groups SET state = 1, done_at = strftime('%s','now') WHERE id IN (SELECT id FROM temp.todo)", [])?;
    tx.execute_batch("DROP TABLE temp.todo;")?;
    Ok(ApplyAll {
        groups,
        kept,
        rejected,
        skipped: 0,
    })
}

/// 폴더 비교로 붙인 표시를 되돌린다 — 이 폴더들 안의 «남김/제외»를 미판정으로, 닫았던 완전 중복
/// 무리는 다시 연다. 휴지통에 이미 간 것은 여기서 안 다룬다(휴지통 화면의 되돌리기).
/// (표시를 되돌린 장수, 다시 연 무리 수)
pub fn unapply_folders(tx: &Transaction, folder_ids: &[i64]) -> rusqlite::Result<(usize, usize)> {
    if folder_ids.is_empty() {
        return Ok((0, 0));
    }
    let list = folder_ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let files = tx.execute(
        &format!(
            "UPDATE files SET culling_flag = 0
             WHERE folder_id IN ({list}) AND trashed_at IS NULL AND culling_flag IN (1, 2)"
        ),
        [],
    )?;
    let groups = tx.execute(
        &format!(
            "UPDATE groups SET state = 0 WHERE kind = 0 AND state = 1
               AND id IN (SELECT m.group_id FROM group_members m JOIN files f ON f.id = m.file_id
                          WHERE f.folder_id IN ({list}))"
        ),
        [],
    )?;
    Ok((files, groups))
}

/// 두 폴더 비교의 «전부» — 짝마다 `apply_set`. 한 트랜잭션이라 화면이 짝마다 명령을 보내며
/// 잠금 없이 두 루프가 얽히던 길이 없다. 못 한 짝은 세어 알린다
#[derive(Debug, Clone, Default, Serialize)]
pub struct PairsApplied {
    pub applied: usize,
    pub failed: usize,
    pub first_error: Option<String>,
    pub kept: usize,
    pub rejected: usize,
}

pub fn apply_pairs(
    tx: &Transaction,
    pairs: &[(Vec<i64>, Vec<i64>)],
) -> rusqlite::Result<PairsApplied> {
    let mut out = PairsApplied::default();
    for (keep, drop) in pairs {
        match apply_trees(tx, keep, drop) {
            Ok(r) => {
                out.applied += 1;
                out.kept += r.kept;
                out.rejected += r.rejected;
            }
            Err(rusqlite::Error::InvalidQuery) => {
                out.failed += 1;
                out.first_error
                    .get_or_insert_with(|| "같은 폴더를 남기고 지울 수는 없습니다".to_string());
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}
