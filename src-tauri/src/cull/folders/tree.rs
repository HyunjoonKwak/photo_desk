use super::{ancestors, in_lib, Disk, FolderIn};
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

pub(super) struct Agg {
    pub(super) info: FolderIn,
    /// 뿌리 기준 상대경로 — 이름이 같은 폴더를 찾는 열쇠
    pub(super) sub: String,
    pub(super) files: i64,
    pub(super) bytes: i64,
    /// culling_flag = 2 인 파일 수
    pub(super) flagged: i64,
    /// culling_flag = 1 인 파일 수
    pub(super) kept: i64,
    /// 전체 해시가 없는 파일 수
    pub(super) unhashed: i64,
    pub(super) hashes: Vec<String>,
    pub(super) all_hashed: bool,
    pub(super) has_children: bool,
}

/// 뿌리는 (볼륨, 볼륨 기준 경로)다 — «연도별»처럼 사진이 바로 아래 없는 폴더는 `folders`
/// 행이 없어서 id 로는 가리킬 수 없다 (실측: 후보1번/연도별을 골랐는데 «없는 폴더»).
fn folders_under(c: &Connection, vol: &str, rel: &str) -> rusqlite::Result<(Vec<Agg>, usize)> {
    folders_under_except(c, vol, rel, None)
}

/// `except` 아래는 뺀다 — 한 뿌리가 다른 뿌리를 품을 때(«2004» ⇔ «2004/주원이사진») 바깥쪽에서 안쪽 나무를 뺀다
pub(super) fn folders_under_except(
    c: &Connection,
    vol: &str,
    rel: &str,
    except: Option<&str>,
) -> rusqlite::Result<(Vec<Agg>, usize)> {
    let esc = crate::db::query::escape_like(rel);
    let mut st = c.prepare(
        "SELECT fo.id, fo.rel_path, fo.area, l.id, l.name, l.rel_path, f.full_hash, f.size,
                f.culling_flag = 2, f.culling_flag = 1
         FROM folders fo
         JOIN libraries l ON l.id = fo.library_id
         LEFT JOIN files f ON f.folder_id = fo.id AND f.trashed_at IS NULL
         WHERE fo.volume_uuid = ?1 AND (fo.rel_path = ?2 OR fo.rel_path LIKE ?3 || '/%' ESCAPE '\\')
         ORDER BY fo.rel_path, f.full_hash",
    )?;
    let mut out: Vec<Agg> = Vec::new();
    let rows = st.query_map(params![vol, rel, esc], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i32>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, Option<bool>>(8)?.unwrap_or(false),
            r.get::<_, Option<bool>>(9)?.unwrap_or(false),
        ))
    })?;
    for row in rows {
        let (id, fo_rel, area, lib_id, lib_name, lib_rel, hash, size, flagged, kept) = row?;
        if out.last().map(|a| a.info.folder_id) != Some(id) {
            let sub = fo_rel
                .strip_prefix(rel)
                .map(|s| s.trim_start_matches('/').to_string())
                .unwrap_or_else(|| fo_rel.clone());
            out.push(Agg {
                info: FolderIn {
                    folder_id: id,
                    library_id: lib_id,
                    library: lib_name,
                    folder: in_lib(&lib_rel, &fo_rel),
                    area,
                },
                sub,
                files: 0,
                bytes: 0,
                flagged: 0,
                kept: 0,
                unhashed: 0,
                hashes: Vec::new(),
                all_hashed: true,
                has_children: false,
            });
        }
        let cur = out.last_mut().unwrap();
        if let Some(size) = size {
            cur.files += 1;
            cur.bytes += size;
            cur.flagged += flagged as i64;
            cur.kept += kept as i64;
            match hash {
                Some(h) => cur.hashes.push(h),
                None => {
                    cur.all_hashed = false;
                    cur.unhashed += 1;
                }
            }
        }
    }
    if let Some(ex) = except {
        out.retain(|a| {
            let full = if a.sub.is_empty() {
                rel.to_string()
            } else if rel.is_empty() {
                a.sub.clone()
            } else {
                format!("{rel}/{}", a.sub)
            };
            !(full == ex || full.starts_with(&format!("{ex}/")))
        });
    }
    // 사진이 바로 아래 없는 폴더 행(휴지통 파일만 가리키거나 빈 것)은 견줄 것이 없다 — 먼저 뺀다.
    // 그다음 디스크에서 사라진 폴더(Finder 에서 지운 것)를 뺀다 — DB 행만 남아 «없는 폴더»를 읽지 않게.
    // 실측(2026-08-30): 다시 스캔 뒤에도 «없는 폴더 N개»가 떴는데, 전부 사진이 0장인 옛 폴더 행이었다
    out.retain(|a| a.files > 0);
    let mut disk = Disk::new();
    let before = out.len();
    out.retain(|a| {
        let rel_full = if a.sub.is_empty() {
            rel.to_string()
        } else if rel.is_empty() {
            a.sub.clone()
        } else {
            format!("{rel}/{}", a.sub)
        };
        disk.dir_exists(vol, &rel_full)
    });
    let missing = before - out.len();
    // 하위 폴더 유무는 이 결과 안에서 안다 — 뿌리 아래 폴더는 전부 여기 들어 있다
    let parents: HashSet<String> = out.iter().flat_map(|a| ancestors(&a.sub)).collect();
    for a in &mut out {
        a.has_children = parents.contains(&a.sub);
    }
    Ok((out, missing))
}

/// 폴더 나무 하나의 «내용» — 하위 폴더까지 합친 해시 다중집합
pub(super) struct Tree {
    /// 이 나무에 든 폴더들의 순번(`Agg` 목록 기준)
    pub(super) members: Vec<usize>,
    pub(super) files: i64,
    pub(super) bytes: i64,
    pub(super) flagged: i64,
    pub(super) kept: i64,
    pub(super) counts: HashMap<String, i64>,
    pub(super) all_hashed: bool,
}

pub(super) fn tree_of(aggs: &[Agg], root: usize) -> Tree {
    let sub = &aggs[root].sub;
    let members: Vec<usize> = (0..aggs.len())
        .filter(|&i| i == root || sub.is_empty() || aggs[i].sub.starts_with(&format!("{sub}/")))
        .collect();
    let mut t = Tree {
        members: Vec::new(),
        files: 0,
        bytes: 0,
        flagged: 0,
        kept: 0,
        counts: HashMap::new(),
        all_hashed: true,
    };
    for &i in &members {
        let g = &aggs[i];
        t.files += g.files;
        t.bytes += g.bytes;
        t.flagged += g.flagged;
        t.kept += g.kept;
        t.all_hashed &= g.all_hashed;
        for h in &g.hashes {
            *t.counts.entry(h.clone()).or_default() += 1;
        }
    }
    t.members = members;
    t
}

/// `inner` 의 파일이 전부 `outer` 에 있나 (같은 내용은 장수까지)
pub(super) fn contained(inner: &Tree, outer: &Tree) -> bool {
    inner.all_hashed
        && inner.files > 0
        && inner
            .counts
            .iter()
            .all(|(h, n)| outer.counts.get(h).copied().unwrap_or(0) >= *n)
}
