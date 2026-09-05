use super::tree::{contained, folders_under_except, tree_of};
use super::FolderIn;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct PairRow {
    /// A 뿌리 아래 폴더 (뿌리 기준 경로). 없으면 B 에만 있는 폴더
    pub a: Option<FolderIn>,
    pub b: Option<FolderIn>,
    pub files_a: i64,
    pub files_b: i64,
    /// 바로 아래 파일이 전부 같다
    pub same: bool,
    /// 이름이 같은 폴더끼리, 양쪽에 똑같이 있는 파일 수
    pub common: i64,
    /// 같은 쪽 하나를 지우면 비는 용량(same 일 때) — 아니면 공통 파일의 용량
    pub bytes: i64,
    /// 이미 제외 표시된 파일 수 — 한쪽이 전부면 그 짝은 «처리됨»
    pub flagged_a: i64,
    pub flagged_b: i64,
    /// «남김»이 붙은 파일 수 — 남김은 결정이라 그쪽은 제외 후보가 아니다
    pub kept_a: i64,
    pub kept_b: i64,
    /// B 쪽 사진이 전부 A 쪽(하위 폴더 포함)에 있다 — B 를 지워도 잃는 것이 없다
    pub b_in_a: bool,
    /// A 쪽 사진이 전부 B 쪽에 있다
    pub a_in_b: bool,
    /// 이 줄이 대표하는 폴더 행들(하위 폴더 포함) — 표시·휴지통으로가 이 목록에 건다
    pub a_ids: Vec<i64>,
    pub b_ids: Vec<i64>,
}

/// 두 폴더 비교의 결과 — 줄들과, 디스크에 없어 뺀 폴더 수, 해시가 없어 견줄 수 없던 사진 수
#[derive(Debug, Clone, Serialize)]
pub struct Compared {
    pub rows: Vec<PairRow>,
    pub missing: usize,
    /// 두 나무 안에서 전체 해시가 아직 없는 사진 — «다시 찾기» 뒤에 들어온 사진. 이게 있으면
    /// 그 폴더는 «똑같음»이 될 수 없다 (실측 2026-08-30: 주원이사진/2004-09-17 29장 전부)
    pub unhashed: usize,
}

/// 두 나무의 해시 없는 사진에 전체 해시를 붙인다 — 비교 화면의 «이 두 폴더 해시 계산».
/// 다시 찾기(전체)를 기다리지 않고 지금 견줄 수 있게. 붙인 수를 돌려준다
pub fn hash_missing(
    db: &crate::db::conn::Db,
    folder_ids: &[i64],
    cancel: &std::sync::atomic::AtomicBool,
) -> crate::db::conn::Result<usize> {
    if folder_ids.is_empty() {
        return Ok(0);
    }
    let list = folder_ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let rows: Vec<(i64, String, String, String)> = db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT fi.id, fo.volume_uuid, fo.rel_path, fi.name FROM files fi JOIN folders fo ON fo.id = fi.folder_id
             WHERE fi.folder_id IN ({list}) AND fi.trashed_at IS NULL AND fi.full_hash IS NULL"
        ))?;
        let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut mounts: HashMap<String, Option<std::path::PathBuf>> = HashMap::new();
    let mut done: Vec<(i64, String, String)> = Vec::new();
    let mut total = 0usize;
    for (id, vol, rel, name) in rows {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let mount = mounts
            .entry(vol.clone())
            .or_insert_with(|| crate::db::volumes::find_mount(&vol))
            .clone();
        let Some(mount) = mount else { continue };
        let path = mount.join(&rel).join(&name);
        let (Ok(q), Ok(f)) = (
            super::super::hash::quick(&path),
            super::super::hash::full(&path),
        ) else {
            continue;
        };
        done.push((id, q, f));
        total += 1;
        if done.len() >= 200 {
            flush_hashes(db, &done)?;
            done.clear();
        }
    }
    flush_hashes(db, &done)?;
    Ok(total)
}

fn flush_hashes(
    db: &crate::db::conn::Db,
    rows: &[(i64, String, String)],
) -> crate::db::conn::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        let mut up =
            tx.prepare("UPDATE files SET quick_hash = ?1, full_hash = ?2 WHERE id = ?3")?;
        for (id, q, f) in rows {
            up.execute(params![q, f, id])?;
        }
        Ok(())
    })
}

/// 두 뿌리가 서로를 품는가 — 같은 폴더가 양쪽 목록에 들어 제 짝이 되는 길을 막는다
pub fn roots_overlap((a_vol, a_rel): (&str, &str), (b_vol, b_rel): (&str, &str)) -> bool {
    if a_vol != b_vol {
        return false;
    }
    let under =
        |root: &str, p: &str| root.is_empty() || p == root || p.starts_with(&format!("{root}/"));
    under(a_rel, b_rel) || under(b_rel, a_rel)
}

/// 폴더 짝 «보기» — 두 나무의 사진을 나란히. 내용이 같은 사진은 서로 `twin` 으로 잇는다
#[derive(Debug, Clone, Serialize)]
pub struct PairPhoto {
    pub file_id: i64,
    pub name: String,
    /// 나무 뿌리 기준 상대 폴더(하위 폴더면 그 이름) — 빈 문자열이면 바로 아래
    pub sub: String,
    pub size: i64,
    pub taken_at: i64,
    pub culling_flag: i32,
    pub library_id: i64,
    pub thumb: Option<String>,
    /// 반대쪽에 있는 같은 내용의 사진 — 없으면 None
    pub twin: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairPhotos {
    pub a: Vec<PairPhoto>,
    pub b: Vec<PairPhoto>,
}

fn photos_in(
    c: &Connection,
    ids: &[i64],
) -> rusqlite::Result<Vec<(PairPhoto, Option<String>, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut st = c.prepare(&format!(
        "SELECT f.id, f.name, fo.rel_path, f.size, f.taken_at, f.culling_flag, fo.library_id, t.rel_path, f.full_hash
         FROM files f JOIN folders fo ON fo.id = f.folder_id
         LEFT JOIN thumbs t ON t.file_id = f.id AND t.state = 1
         WHERE f.folder_id IN ({list}) AND f.trashed_at IS NULL
         ORDER BY fo.rel_path, f.name"
    ))?;
    let rows = st.query_map([], |r| {
        Ok((
            PairPhoto {
                file_id: r.get(0)?,
                name: r.get(1)?,
                sub: String::new(),
                size: r.get(3)?,
                taken_at: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                culling_flag: r.get(5)?,
                library_id: r.get(6)?,
                thumb: r.get(7)?,
                twin: None,
            },
            r.get::<_, Option<String>>(8)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    rows.collect()
}

/// 두 나무의 사진 — 같은 내용끼리 1:1 로 짝짓는다(장수까지). 폴더 경로는 뿌리 아래만 보인다
pub fn pair_photos(c: &Connection, a_ids: &[i64], b_ids: &[i64]) -> rusqlite::Result<PairPhotos> {
    let mut a = photos_in(c, a_ids)?;
    let mut b = photos_in(c, b_ids)?;
    let strip = |rows: &mut Vec<(PairPhoto, Option<String>, String)>| {
        // 뿌리 = 가장 짧은 폴더 경로
        let root = rows
            .iter()
            .map(|r| r.2.clone())
            .min_by_key(|p| p.len())
            .unwrap_or_default();
        for r in rows.iter_mut() {
            r.0.sub =
                r.2.strip_prefix(&root)
                    .map(|s| s.trim_start_matches('/').to_string())
                    .unwrap_or_default();
        }
    };
    strip(&mut a);
    strip(&mut b);
    let mut by_hash: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (_, h, _)) in b.iter().enumerate() {
        if let Some(h) = h {
            by_hash.entry(h.clone()).or_default().push(i);
        }
    }
    for (pa, h, _) in a.iter_mut() {
        let Some(h) = h else { continue };
        if let Some(list) = by_hash.get_mut(h) {
            if let Some(i) = list.pop() {
                pa.twin = Some(b[i].0.file_id);
                b[i].0.twin = Some(pa.file_id);
            }
        }
    }
    Ok(PairPhotos {
        a: a.into_iter().map(|r| r.0).collect(),
        b: b.into_iter().map(|r| r.0).collect(),
    })
}

/// 두 폴더(와 그 아래)를 견준다 — «후보1번/연도별»과 «후보2번»처럼.
///
/// 폴더는 **나무째** 본다: 하위 폴더까지 합친 내용으로 «B 쪽이 A 에 다 있다 / A 쪽이 B 에 다
/// 있다 / 둘 다(똑같다)»를 가린다. 한쪽이 다른 쪽에 다 들어 있으면 그 나무는 한 줄로 끝나고
/// 하위 폴더는 따로 안 나온다 — 실측: `2011-04-24(주말농장-2번째)` 는 후보1번에만 «블로그»
/// 하위 폴더가 더 있어 «똑같음»이 아니었지만 후보2번 쪽 191장은 전부 후보1번에 있었다.
/// 어느 쪽도 다른 쪽을 품지 못하면 «부분»으로 적고 하위 폴더는 저마다 제 줄로 내려간다.
pub fn compare_two(
    c: &Connection,
    (a_vol, a_rel): (&str, &str),
    (b_vol, b_rel): (&str, &str),
) -> rusqlite::Result<Compared> {
    if a_vol == b_vol && a_rel == b_rel {
        return Err(rusqlite::Error::InvalidQuery);
    }
    // 한쪽이 다른 쪽을 품으면 바깥쪽에서 안쪽 나무를 뺀다 — «공용/2004» ⇔ «공용/2004/주원이사진»처럼
    // 같은 폴더 안에 사본 갈래가 있는 경우 (사용자 요청 2026-08-30)
    let same_vol = a_vol == b_vol;
    let b_in_a_root = same_vol && (a_rel.is_empty() || b_rel.starts_with(&format!("{a_rel}/")));
    let a_in_b_root = same_vol && (b_rel.is_empty() || a_rel.starts_with(&format!("{b_rel}/")));
    let (a, miss_a) = folders_under_except(c, a_vol, a_rel, b_in_a_root.then_some(b_rel))?;
    let (b, miss_b) = folders_under_except(c, b_vol, b_rel, a_in_b_root.then_some(a_rel))?;
    let mut b_by_sub: HashMap<&str, usize> = HashMap::new();
    for (i, g) in b.iter().enumerate() {
        b_by_sub.entry(g.sub.as_str()).or_insert(i);
    }
    let mut used_a = vec![false; a.len()];
    let mut used_b = vec![false; b.len()];
    // (정렬 열쇠 = 뿌리 기준 경로, 줄)
    let mut out: Vec<(String, PairRow)> = Vec::new();
    // 경로 순으로 — 위 폴더가 먼저 나와 나무째 짝지어지면 아래는 건너뛴다
    let mut order: Vec<usize> = (0..a.len()).collect();
    order.sort_by(|&x, &y| a[x].sub.cmp(&a[y].sub));
    for ia in order {
        if used_a[ia] {
            continue;
        }
        let ga = &a[ia];
        let Some(&ib) = b_by_sub.get(ga.sub.as_str()) else {
            // 이름이 같은 짝이 없다 — 사진이 있으면 «A 에만»
            if ga.files > 0 {
                used_a[ia] = true;
                out.push((
                    ga.sub.clone(),
                    PairRow {
                        a: Some(ga.info.clone()),
                        b: None,
                        files_a: ga.files,
                        files_b: 0,
                        same: false,
                        common: 0,
                        bytes: 0,
                        flagged_a: ga.flagged,
                        flagged_b: 0,
                        kept_a: ga.kept,
                        kept_b: 0,
                        b_in_a: false,
                        a_in_b: false,
                        a_ids: vec![ga.info.folder_id],
                        b_ids: Vec::new(),
                    },
                ));
            }
            continue;
        };
        if used_b[ib] {
            continue;
        }
        let ta = tree_of(&a, ia);
        let tb = tree_of(&b, ib);
        let b_in_a = contained(&tb, &ta);
        let a_in_b = contained(&ta, &tb);
        if b_in_a || a_in_b {
            // 나무째 한 줄 — 하위 폴더는 이 줄이 대표한다
            for &i in &ta.members {
                used_a[i] = true;
            }
            for &i in &tb.members {
                used_b[i] = true;
            }
            let common = ta.files.min(tb.files);
            out.push((
                ga.sub.clone(),
                PairRow {
                    a: Some(ga.info.clone()),
                    b: Some(b[ib].info.clone()),
                    files_a: ta.files,
                    files_b: tb.files,
                    same: b_in_a && a_in_b,
                    common,
                    // 지울 수 있는 쪽의 용량 — 둘 다면 작은 쪽
                    bytes: if b_in_a && a_in_b {
                        ta.bytes.min(tb.bytes)
                    } else if b_in_a {
                        tb.bytes
                    } else {
                        ta.bytes
                    },
                    flagged_a: ta.flagged,
                    flagged_b: tb.flagged,
                    kept_a: ta.kept,
                    kept_b: tb.kept,
                    b_in_a,
                    a_in_b,
                    a_ids: ta.members.iter().map(|&i| a[i].info.folder_id).collect(),
                    b_ids: tb.members.iter().map(|&i| b[i].info.folder_id).collect(),
                },
            ));
            continue;
        }
        // 부분 — 바로 아래 파일끼리 겹치는 수. 하위 폴더는 저마다 제 줄로
        used_a[ia] = true;
        used_b[ib] = true;
        let gb = &b[ib];
        if ga.files == 0 && gb.files == 0 {
            continue;
        }
        let mut counts: HashMap<&str, i64> = HashMap::new();
        for h in &gb.hashes {
            *counts.entry(h.as_str()).or_default() += 1;
        }
        let mut common = 0i64;
        let mut bytes = 0i64;
        let per = if ga.files > 0 { ga.bytes / ga.files } else { 0 };
        for h in &ga.hashes {
            if let Some(n) = counts.get_mut(h.as_str()) {
                if *n > 0 {
                    *n -= 1;
                    common += 1;
                    bytes += per;
                }
            }
        }
        out.push((
            ga.sub.clone(),
            PairRow {
                a: Some(ga.info.clone()),
                b: Some(gb.info.clone()),
                files_a: ga.files,
                files_b: gb.files,
                same: false,
                common,
                bytes,
                flagged_a: ga.flagged,
                flagged_b: gb.flagged,
                kept_a: ga.kept,
                kept_b: gb.kept,
                b_in_a: false,
                a_in_b: false,
                a_ids: vec![ga.info.folder_id],
                b_ids: vec![gb.info.folder_id],
            },
        ));
    }
    for (i, gb) in b.iter().enumerate() {
        if used_b[i] || gb.files == 0 {
            continue;
        }
        out.push((
            gb.sub.clone(),
            PairRow {
                a: None,
                b: Some(gb.info.clone()),
                files_a: 0,
                files_b: gb.files,
                same: false,
                common: 0,
                bytes: 0,
                flagged_a: 0,
                flagged_b: gb.flagged,
                kept_a: 0,
                kept_b: gb.kept,
                b_in_a: false,
                a_in_b: false,
                a_ids: Vec::new(),
                b_ids: vec![gb.info.folder_id],
            },
        ));
    }
    // 경로 순 — Finder 를 나란히 놓은 것처럼 읽힌다 (사용자 지적: «오름차순도 내림차순도 아니다»)
    out.sort_by(|x, y| x.0.cmp(&y.0));
    let unhashed = a.iter().chain(b.iter()).map(|g| g.unhashed).sum::<i64>() as usize;
    Ok(Compared {
        rows: out.into_iter().map(|(_, r)| r).collect(),
        missing: miss_a + miss_b,
        unhashed,
    })
}
