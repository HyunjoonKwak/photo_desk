//! 폴더 비교 — 내용이 완전히 같은 폴더들.
//!
//! 사용자의 말: «후보1번에도 있고 후보2번에도 있고 공용에도 있으면 셋을 한꺼번에
//! 보여 주고, 둘이면 둘. 폴더가 완전히 같은데 사진은 뭐하러 보여 주나.»
//!
//! 폴더의 «서명» = 바로 아래 파일들의 전체 해시를 정렬해 이어 붙인 것. 서명이 같은
//! 폴더끼리 한 묶음이다. 파일 하나라도 해시가 없으면(크기가 유일해 후보조차 아니었던
//! 파일) 그 폴더는 어디와도 같을 수 없으니 뺀다. 하위 폴더가 있는 폴더는 묶지 않는다 —
//! 하위는 저마다 따로 비교한다.

use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

mod apply;
mod compare;
mod tree;

pub use apply::{apply_pair, apply_pairs, apply_trees, unapply_folders, PairsApplied};
pub use compare::{compare_two, hash_missing, pair_photos, Compared, PairPhotos};

#[derive(Debug, Clone, Serialize)]
pub struct FolderIn {
    pub folder_id: i64,
    pub library_id: i64,
    pub library: String,
    /// 라이브러리 기준 경로
    pub folder: String,
    pub area: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderSet {
    /// 같은 내용의 폴더들 — 정착 구역이 앞에
    pub folders: Vec<FolderIn>,
    /// `folders` 와 같은 순서 — 각 폴더 나무의 폴더 행 id 들(하위 포함). 표시·휴지통으로가 이 목록에 건다
    pub ids: Vec<Vec<i64>>,
    pub files: i64,
    /// 폴더 하나의 용량 — 하나만 남기면 (n-1)배가 빈다
    pub bytes: i64,
    /// 이 묶음의 파일 중 제외 표시가 아직 없는 것이 있나
    pub pending: bool,
    /// 묶음 안에서 제외 표시된 파일 수 — «표시한 N장 치우기»
    pub flagged: i64,
}

fn in_lib(lib_rel: &str, rel: &str) -> String {
    rel.strip_prefix(lib_rel)
        .map(|s| s.trim_start_matches('/'))
        .unwrap_or(rel)
        .to_string()
}

/// 하위 폴더 행이 있는 (볼륨, 경로) 집합. `folders.parent_id`는 스캐너가 채우지 않아
/// 그걸로 걸러 봐야 아무것도 안 걸러진다 — 경로의 위 폴더를 셈해서 만든다 (리뷰 H5)
fn parents_with_children(c: &Connection) -> rusqlite::Result<HashSet<(String, String)>> {
    let mut st = c.prepare("SELECT volume_uuid, rel_path FROM folders")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashSet::new();
    for row in rows {
        let (vol, rel) = row?;
        // 위 폴더 전부 — 중간 폴더는 사진이 바로 아래 없으면 행이 없어서 바로 위만 보면 놓친다
        for p in ancestors(&rel) {
            out.insert((vol.clone(), p));
        }
    }
    Ok(out)
}

/// `a/b/c` → `a/b`, `a`, `` (뿌리)
fn ancestors(rel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = rel;
    while let Some((p, _)) = cur.rsplit_once('/') {
        out.push(p.to_string());
        cur = p;
    }
    if !rel.is_empty() {
        out.push(String::new());
    }
    out
}

/// 볼륨이 지금 붙어 있나, 폴더가 디스크에 아직 있나 — 마운트는 볼륨마다 한 번만 찾는다.
/// Finder 에서 지운 폴더의 행이 DB 에 남아 있을 수 있다(감시는 «폴더가 안 보이면 지우지
/// 않는다», 리뷰 C2). 그런 폴더를 견주면 «없는 폴더를 읽는다»가 된다 (실측 2026-08-30: 269개)
struct Disk(HashMap<String, Option<std::path::PathBuf>>);
impl Disk {
    fn new() -> Self {
        Disk(HashMap::new())
    }
    fn mount(&mut self, vol: &str) -> Option<std::path::PathBuf> {
        self.0
            .entry(vol.to_string())
            .or_insert_with(|| crate::db::volumes::find_mount(vol))
            .clone()
    }
    fn online(&mut self, vol: &str) -> bool {
        self.mount(vol).is_some()
    }
    fn dir_exists(&mut self, vol: &str, rel: &str) -> bool {
        self.mount(vol)
            .map(|m| m.join(rel).is_dir())
            .unwrap_or(false)
    }
}

/// 내용이 완전히 같은 폴더 묶음들 — **나무째** 본다(하위 폴더까지 합친 내용). 위 폴더끼리 같으면
/// 아래 폴더는 따로 안 나온다. 경로 순.
///
/// 실측(2026-08-30): 바로 아래 파일만 보던 때는 하위 폴더가 있는 폴더를 통째로 뺐고, 그래서
/// 껍질 벗기듯 여러 번 돌아야 했다 — 두 폴더 비교와 같은 나무 판정으로 맞춘다
pub fn identical_sets(c: &Connection, limit: usize) -> rusqlite::Result<Vec<FolderSet>> {
    struct Row {
        info: FolderIn,
        vol: String,
        rel: String,
        n: i64,
        bytes: i64,
        pend: i64,
        flagged: i64,
        nohash: i64,
        hashes: Vec<String>,
    }
    let mut st = c.prepare(
        "WITH tot AS (
           SELECT folder_id, COUNT(*) n, SUM(full_hash IS NULL) nohash, SUM(size) bytes,
                  SUM(culling_flag = 0) pend, SUM(culling_flag = 2) flagged
           FROM files WHERE trashed_at IS NULL GROUP BY folder_id),
         sig AS (
           SELECT folder_id, group_concat(full_hash, ',') s
           FROM (SELECT folder_id, full_hash FROM files
                 WHERE trashed_at IS NULL AND full_hash IS NOT NULL
                 ORDER BY folder_id, full_hash)
           GROUP BY folder_id)
         SELECT fo.id, fo.library_id, l.name, l.rel_path, fo.rel_path, fo.area,
                t.n, t.bytes, t.pend, t.flagged, t.nohash, sig.s, fo.volume_uuid
         FROM folders fo
         JOIN libraries l ON l.id = fo.library_id
         JOIN tot t ON t.folder_id = fo.id
         LEFT JOIN sig ON sig.folder_id = fo.id",
    )?;
    let mut rows: Vec<Row> = st
        .query_map([], |r| {
            let lib_rel: String = r.get(3)?;
            let rel: String = r.get(4)?;
            let s: Option<String> = r.get(11)?;
            Ok(Row {
                info: FolderIn {
                    folder_id: r.get(0)?,
                    library_id: r.get(1)?,
                    library: r.get(2)?,
                    folder: in_lib(&lib_rel, &rel),
                    area: r.get(5)?,
                },
                vol: r.get(12)?,
                rel,
                n: r.get(6)?,
                bytes: r.get(7)?,
                pend: r.get(8)?,
                flagged: r.get(9)?,
                nohash: r.get(10)?,
                hashes: s
                    .map(|s| s.split(',').map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // 사진이 바로 아래 없는 중간 폴더(«2004/주원이사진»)는 행이 없다 — 가상 마디로 세운다. 안 그러면
    // 그 아래 날짜 폴더가 하나씩 따로 나온다 (사용자 지적 2026-08-30). 라이브러리 뿌리 안쪽만
    let lib_roots: HashMap<i64, String> = {
        let mut st = c.prepare("SELECT id, rel_path FROM libraries")?;
        let v: Vec<(i64, String)> = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        v.into_iter().collect()
    };
    {
        let existing: HashSet<(String, String)> = rows
            .iter()
            .map(|r| (r.vol.clone(), r.rel.clone()))
            .collect();
        let mut virt: HashMap<(String, String), FolderIn> = HashMap::new();
        for r in &rows {
            let Some(lib_rel) = lib_roots.get(&r.info.library_id) else {
                continue;
            };
            for anc in ancestors(&r.rel) {
                if anc.len() <= lib_rel.len()
                    || !(lib_rel.is_empty() || anc.starts_with(&format!("{lib_rel}/")))
                {
                    continue;
                }
                if existing.contains(&(r.vol.clone(), anc.clone())) {
                    continue;
                }
                virt.entry((r.vol.clone(), anc.clone()))
                    .or_insert_with(|| FolderIn {
                        // 행이 없으니 id 는 첫 후손의 것을 빌린다(화면 열쇠용) — 실제 표시는 ids 목록에 건다
                        folder_id: r.info.folder_id,
                        library_id: r.info.library_id,
                        library: r.info.library.clone(),
                        folder: in_lib(lib_rel, &anc),
                        area: r.info.area,
                    });
            }
        }
        for ((vol, rel), info) in virt {
            rows.push(Row {
                info,
                vol,
                rel,
                n: 0,
                bytes: 0,
                pend: 0,
                flagged: 0,
                nohash: 0,
                hashes: Vec::new(),
            });
        }
    }

    // 나무 합치기 — 폴더마다 (해시 → 장수) 다중집합을 위 폴더들에 더한다
    let index: HashMap<(String, String), usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.vol.clone(), r.rel.clone()), i))
        .collect();
    struct Tree {
        counts: HashMap<String, i64>,
        files: i64,
        bytes: i64,
        pend: i64,
        flagged: i64,
        nohash: i64,
        ids: Vec<i64>,
    }
    let mut trees: Vec<Tree> = rows
        .iter()
        .map(|r| {
            let mut counts: HashMap<String, i64> = HashMap::new();
            for h in &r.hashes {
                *counts.entry(h.clone()).or_default() += 1;
            }
            // 가상 마디(n == 0, 해시 없음)는 제 id 를 넣지 않는다 — 후손들이 채운다
            Tree {
                counts,
                files: r.n,
                bytes: r.bytes,
                pend: r.pend,
                flagged: r.flagged,
                nohash: r.nohash,
                ids: if r.n > 0 {
                    vec![r.info.folder_id]
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    for row in &rows {
        if row.n == 0 {
            continue; // 가상 마디는 더할 것이 없다
        }
        for anc in ancestors(&row.rel) {
            if let Some(&j) = index.get(&(row.vol.clone(), anc)) {
                let (own_counts, own) = (
                    row.hashes.clone(),
                    (
                        row.n,
                        row.bytes,
                        row.pend,
                        row.flagged,
                        row.nohash,
                        row.info.folder_id,
                    ),
                );
                let t = &mut trees[j];
                for h in own_counts {
                    *t.counts.entry(h).or_default() += 1;
                }
                t.files += own.0;
                t.bytes += own.1;
                t.pend += own.2;
                t.flagged += own.3;
                t.nohash += own.4;
                t.ids.push(own.5);
            }
        }
    }

    // 서명 = 정렬한 (해시:장수). 해시 없는 파일이 하나라도 있으면 어디와도 같을 수 없다
    let mut disk = Disk::new();
    let mut by_sig: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in trees.iter().enumerate() {
        if t.files == 0 || t.nohash > 0 {
            continue;
        }
        if !disk.online(&rows[i].vol) || !disk.dir_exists(&rows[i].vol, &rows[i].rel) {
            continue;
        }
        let mut parts: Vec<String> = t.counts.iter().map(|(h, n)| format!("{h}:{n}")).collect();
        parts.sort();
        by_sig.entry(parts.join(",")).or_default().push(i);
    }
    // 위 폴더끼리 같은 묶음이 있으면 그 아래는 안 낸다 — 얕은 것부터 보며 덮인 자리를 적는다
    // 한 묶음 안에서 제 후손은 뺀다 — 하위 폴더 하나뿐인 폴더는 그 하위 폴더와 서명이 같아 같이 묶인다
    let mut sets: Vec<Vec<usize>> = by_sig
        .into_values()
        .map(|v| {
            let mut v = v;
            v.sort_by_key(|&i| rows[i].rel.len());
            let mut kept: Vec<usize> = Vec::new();
            for i in v {
                let is_desc = kept.iter().any(|&k| {
                    rows[k].vol == rows[i].vol
                        && rows[i].rel.starts_with(&format!("{}/", rows[k].rel))
                });
                if !is_desc {
                    kept.push(i);
                }
            }
            kept
        })
        .filter(|v| v.len() >= 2)
        .collect();
    sets.sort_by_key(|v| {
        v.iter()
            .map(|&i| rows[i].rel.matches('/').count())
            .min()
            .unwrap_or(0)
    });
    let mut covered: Vec<(String, String)> = Vec::new(); // (vol, rel) — 이 아래는 덮였다
    let is_covered = |vol: &str, rel: &str, covered: &[(String, String)]| {
        covered
            .iter()
            .any(|(v, r)| v == vol && (rel == r || rel.starts_with(&format!("{r}/"))))
    };
    let mut out: Vec<FolderSet> = Vec::new();
    for members in sets {
        if members
            .iter()
            .all(|&i| is_covered(&rows[i].vol, &rows[i].rel, &covered))
        {
            continue;
        }
        for &i in &members {
            covered.push((rows[i].vol.clone(), rows[i].rel.clone()));
        }
        let mut fs: Vec<(FolderIn, Vec<i64>)> = members
            .iter()
            .map(|&i| (rows[i].info.clone(), trees[i].ids.clone()))
            .collect();
        // 정착 구역이 앞에, 그다음은 라이브러리·경로 순 — 남길 것이 맨 앞
        fs.sort_by(|(a, _), (b, _)| {
            let sa = !(a.area == 1 || a.area == 2);
            let sb = !(b.area == 1 || b.area == 2);
            sa.cmp(&sb)
                .then(a.library_id.cmp(&b.library_id))
                .then(a.folder.cmp(&b.folder))
        });
        let first = members[0];
        let pending = members.iter().any(|&i| trees[i].pend > 0);
        let flagged = members.iter().map(|&i| trees[i].flagged).sum();
        let (folders, ids): (Vec<FolderIn>, Vec<Vec<i64>>) = fs.into_iter().unzip();
        out.push(FolderSet {
            folders,
            ids,
            files: trees[first].files,
            bytes: trees[first].bytes,
            pending,
            flagged,
        });
    }
    // 경로 순 — 묶음의 이름은 정착 구역 폴더(맨 앞)의 경로
    out.sort_by(|a, b| {
        a.folders[0]
            .folder
            .cmp(&b.folders[0].folder)
            .then(a.folders[0].library_id.cmp(&b.folders[0].library_id))
    });
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests;
