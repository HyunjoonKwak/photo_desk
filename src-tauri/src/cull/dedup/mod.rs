//! 완전 중복 찾기 — 바이트가 같은 파일들.
//!
//! 1차 구역 18,049개를 실측했을 때 완전 중복이 1,599개(15.1GB)였다. 가족 사진을
//! 여러 경로로 받거나, 구글포토 누락본을 다시 내려받거나 하면 이렇게 쌓인다.
//!
//! 판정은 [`super::hash`]의 3단계를 따른다. 크기로 후보를 좁히고, 빠른 해시로
//! 다시 좁힌 다음, 전체 해시가 같은 것만 한 그룹으로 묶는다.
//!
//! 4단계 «메타데이터만 다른 사본»(2026-08-30): 촬영일시 EXIF 를 나중에 써 넣은 사본은
//! 106바이트가 늘어 1단계에서 빠진다 (실측: 하와이 1,081장 — 내사진 쪽 2026-07 에 써 넣음).
//! 이름·픽셀 크기가 같고 파일 크기만 조금 다른 짝은 그림 데이터만 해시해 비교한다.

use crate::cull::hash;
use crate::db::conn::{Db, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DedupProgress {
    /// 크기가 겹쳐 확인이 필요한 파일 수
    pub candidates: usize,
    pub hashed: usize,
    pub groups: usize,
    /// 중복을 정리하면 확보되는 용량
    pub reclaimable: i64,
    /// 어느 단계인가 — "quick"(앞뒤 128KB) 또는 "full"(파일 전체). 전체 해시는
    /// 오래 걸리는데 숫자가 안 바뀌면 멈춘 줄 안다 (실측: 191,000에서 «멈췄다»).
    pub phase: &'static str,
    /// 전체 해시 — 대상 수, 읽은 수, 읽은 바이트
    pub full_total: usize,
    pub full_done: usize,
    pub full_bytes: i64,
    /// 디스크가 안 꽂혀 있어 뺀 후보 수
    pub offline: usize,
    /// 4단계 그림 해시 — 대상 수, 읽은 수
    pub image_total: usize,
    pub image_done: usize,
}

/// 4단계 후보의 파일 크기 차 상한. 촬영일시 EXIF 는 100바이트 남짓, XMP·ICC·미리보기를
/// 다 넣어도 수십 KB 다. 이보다 크게 다르면 다시 인코딩된 것으로 보고 재지 않는다
pub const TWIN_SLACK: i64 = 256 * 1024;

/// 그룹을 쓸 때 필요한 것 — 구성원마다 SELECT 하지 않게
#[derive(Clone)]
struct Info {
    size: i64,
    area: i32,
    taken_at: i64,
    full: Option<String>,
    flag: i32,
}

/// 4단계 후보 — 이름·픽셀 크기가 같은 다른 파일이 있고, 크기만 조금 다른 것
struct Twin {
    id: i64,
    path: PathBuf,
    volume_uuid: String,
    image: Option<String>,
    info: Info,
}

struct Cand {
    id: i64,
    /// 볼륨 기준 상대경로. 실제 경로는 그 볼륨의 마운트를 앞에 붙여 만든다.
    path: PathBuf,
    volume_uuid: String,
    size: i64,
    /// 지난 스캔이 남긴 해시 — 있으면 다시 읽지 않는다. 파일이 바뀌면 스캔이 지운다.
    quick: Option<String>,
    full: Option<String>,
    /// 대표를 고를 때 쓴다 — 쓰기 잠금 안에서 구성원마다 SELECT 하지 않게 (리뷰 H8)
    area: i32,
    taken_at: i64,
    /// 폴더 비교 등이 먼저 붙인 표시 — 남김(1)이면 대표로 우선한다
    flag: i32,
}

/// 읽은 해시를 남긴다 — 다음 스캔은 이 파일들을 다시 읽지 않는다.
fn persist(db: &Db, column: &str, rows: &[(i64, String)]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let sql = format!("UPDATE files SET {column}=?1 WHERE id=?2");
    db.transaction(|tx| {
        let mut up = tx.prepare(&sql)?;
        for (id, h) in rows {
            up.execute(rusqlite::params![h, id])?;
        }
        Ok(())
    })
}

/// 완전 중복을 찾아 `groups`/`group_members`에 기록한다.
///
/// 남길 한 장은 정하지 않는다 — 사용자가 고른다. 다만 자동 선정을 돕도록
/// 가장 이른 촬영일을 가진 것에 `is_best`를 표시한다.
/// 등록한 라이브러리 **전부**를 가로질러 찾는다.
///
/// 볼륨을 하나로 제한하지 않는 이유: 옛 백업 디스크와 운영 디스크 사이의
/// 중복이야말로 가장 크게 확보된다. 대신 파일마다 자기 볼륨의 마운트를 찾아
/// 경로를 푼다 — 디스크가 빠져 있으면 그 파일은 그냥 건너뛴다.
pub fn scan(
    db: &Db,
    cancel: Arc<AtomicBool>,
    on_progress: impl Fn(&DedupProgress) + Sync + Send,
) -> Result<DedupProgress> {
    // 1단계: 크기가 겹치는 것만 후보로. 유일한 크기는 볼 것도 없다.
    let mut cands: Vec<Cand> = db.read(|c| {
        let mut st = c.prepare(
            "SELECT fi.id, fo.rel_path, fi.name, fi.size, fo.volume_uuid, fi.quick_hash, fi.full_hash,
                    fo.area, fi.taken_at, fi.culling_flag
             FROM files fi JOIN folders fo ON fo.id = fi.folder_id
             WHERE fi.size > 0 AND fi.trashed_at IS NULL
               AND fi.size IN (SELECT size FROM files WHERE trashed_at IS NULL GROUP BY size HAVING COUNT(*) > 1)",
        )?;
        let it = st.query_map([], |r| {
            let dir: String = r.get(1)?;
            let name: String = r.get(2)?;
            Ok(Cand {
                id: r.get(0)?,
                path: PathBuf::from(crate::media::cache::rel_path(&dir, &name)),
                volume_uuid: r.get(4)?,
                size: r.get(3)?,
                quick: r.get(5)?,
                full: r.get(6)?,
                area: r.get(7)?,
                taken_at: r.get(8)?,
                flag: r.get(9)?,
            })
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;

    // 볼륨마다 마운트를 한 번만 찾는다. 파일마다 찾으면 수만 번 syscall이다.
    let mounts: HashMap<String, PathBuf> = cands
        .iter()
        .map(|c| c.volume_uuid.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .filter_map(|u| crate::db::volumes::find_mount(&u).map(|m| (u, m)))
        .collect();
    // 안 꽂힌 디스크의 파일은 저장된 해시가 있어도 뺀다 — 그것이 대표가 되면 살아 있는
    // 쪽이 제외되고, 그 디스크는 다시 안 올 수도 있다 (리뷰 C10)
    let before = cands.len();
    cands.retain(|c| mounts.contains_key(&c.volume_uuid));
    let offline = before - cands.len();
    let full_path = |c: &Cand| mounts.get(&c.volume_uuid).map(|m| m.join(&c.path));

    let progress = Arc::new(Mutex::new(DedupProgress {
        candidates: cands.len(),
        offline,
        ..Default::default()
    }));
    on_progress(&progress.lock().unwrap().clone());
    // 크기가 겹치는 것이 없어도 끝내지 않는다 — 4단계(메타데이터만 다른 사본)는 따로 후보를 고른다

    // 2단계: 빠른 해시 (파일당 128KB만 읽는다). 지난번 것이 있으면 그대로 쓴다.
    let counter = AtomicUsize::new(0);
    let new_quick = Mutex::new(Vec::<(i64, String)>::new());
    let quick: Vec<(i64, i64, String)> = cands
        .par_iter()
        .filter_map(|c| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let q = match &c.quick {
                Some(q) => q.clone(),
                None => {
                    let q = hash::quick(full_path(c)?).ok()?;
                    new_quick.lock().unwrap().push((c.id, q.clone()));
                    q
                }
            };
            let n = counter.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(500) {
                let mut p = progress.lock().unwrap();
                p.phase = "quick";
                p.hashed = n;
                on_progress(&p.clone());
            }
            Some((c.id, c.size, q))
        })
        .collect();
    persist(db, "quick_hash", &new_quick.into_inner().unwrap())?;
    if cancel.load(Ordering::Relaxed) {
        // 반쪽 결과로 묶으면 엉뚱한 그룹이 된다 — 해시만 남기고 그룹은 손대지 않는다
        return Ok(progress.lock().unwrap().clone());
    }
    // (크기, 빠른해시)가 같은 것끼리 묶는다. 혼자면 중복이 아니다.
    let mut buckets: HashMap<(i64, String), Vec<i64>> = HashMap::new();
    for (id, size, q) in quick {
        buckets.entry((size, q)).or_default().push(id);
    }
    let by_id: HashMap<i64, &Cand> = cands.iter().map(|c| (c.id, c)).collect();
    let need_full: Vec<&Cand> = buckets
        .values()
        .filter(|v| v.len() > 1)
        .flatten()
        .filter_map(|id| by_id.get(id).copied())
        .collect();
    // 3단계: 전체 해시 — 앞뒤가 같은 것만. 옛 백업 디스크와 운영 디스크가 같이
    // 등록돼 있으면 여기가 수십만 장·수백 GB다. 진행을 보내고, 200장마다 저장해
    // 멈춰도 읽은 만큼은 남긴다.
    {
        let mut p = progress.lock().unwrap();
        p.phase = "full";
        p.hashed = cands.len();
        p.full_total = need_full.len();
        on_progress(&p.clone());
    }
    let full_done = AtomicUsize::new(0);
    let full_bytes = AtomicI64::new(0);
    let pending = Mutex::new(Vec::<(i64, String)>::new());
    let full_total = need_full.len();
    let full_hashes: Vec<(i64, String)> = need_full
        .par_iter()
        .filter_map(|c| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let h = match &c.full {
                Some(h) => h.clone(),
                None => {
                    let h = hash::full(full_path(c)?).ok()?;
                    full_bytes.fetch_add(c.size, Ordering::Relaxed);
                    let mut pend = pending.lock().unwrap();
                    pend.push((c.id, h.clone()));
                    if pend.len() >= 200 {
                        let batch = std::mem::take(&mut *pend);
                        drop(pend);
                        let _ = persist(db, "full_hash", &batch);
                    }
                    h
                }
            };
            let n = full_done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(25) || n == full_total {
                let mut p = progress.lock().unwrap();
                p.full_done = n;
                p.full_bytes = full_bytes.load(Ordering::Relaxed);
                on_progress(&p.clone());
            }
            Some((c.id, h))
        })
        .collect();
    persist(db, "full_hash", &pending.into_inner().unwrap())?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(progress.lock().unwrap().clone());
    }
    // 전체 해시가 같은 것끼리 그룹
    let mut final_groups: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, h) in full_hashes {
        final_groups.entry(h).or_default().push(id);
    }
    let mut info: HashMap<i64, Info> = cands
        .iter()
        .map(|c| {
            (
                c.id,
                Info {
                    size: c.size,
                    area: c.area,
                    taken_at: c.taken_at,
                    full: c.full.clone(),
                    flag: c.flag,
                },
            )
        })
        .collect();
    // 전체 해시를 방금 읽은 것은 cands.full 에 없다 — 그룹 사유 판정에 쓰이므로 채운다
    for (h, ids) in &final_groups {
        for id in ids {
            if let Some(i) = info.get_mut(id) {
                i.full = Some(h.clone());
            }
        }
    }

    // 4단계: 메타데이터만 다른 사본 — 그림 데이터만 해시한다
    let mut twins = twin_candidates(db)?;
    let twin_mounts: HashMap<String, PathBuf> = twins
        .iter()
        .map(|t| t.volume_uuid.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .filter_map(|u| crate::db::volumes::find_mount(&u).map(|m| (u, m)))
        .collect();
    twins.retain(|t| twin_mounts.contains_key(&t.volume_uuid));
    {
        let mut p = progress.lock().unwrap();
        p.phase = "image";
        p.image_total = twins.len();
        on_progress(&p.clone());
    }
    let image_done = AtomicUsize::new(0);
    let pending = Mutex::new(Vec::<(i64, String)>::new());
    let image_total = twins.len();
    let image_hashes: Vec<(i64, String)> = twins
        .par_iter()
        .filter_map(|t| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let h = match &t.image {
                Some(h) => Some(h.clone()),
                None => {
                    let path = twin_mounts.get(&t.volume_uuid)?.join(&t.path);
                    let h = hash::image(path).ok()??;
                    let mut pend = pending.lock().unwrap();
                    pend.push((t.id, h.clone()));
                    if pend.len() >= 200 {
                        let batch = std::mem::take(&mut *pend);
                        drop(pend);
                        let _ = persist(db, "image_hash", &batch);
                    }
                    Some(h)
                }
            };
            let n = image_done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(25) || n == image_total {
                let mut p = progress.lock().unwrap();
                p.image_done = n;
                on_progress(&p.clone());
            }
            h.map(|h| (t.id, h))
        })
        .collect();
    persist(db, "image_hash", &pending.into_inner().unwrap())?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(progress.lock().unwrap().clone());
    }
    for t in &twins {
        info.entry(t.id).or_insert_with(|| t.info.clone());
    }
    let mut twin_groups: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, h) in image_hashes {
        twin_groups.entry(h).or_default().push(id);
    }

    // 바이트가 같은 무리와 그림이 같은 무리를 합친다 — 한 사진이 양쪽에 걸치면 한 그룹
    let comps = merge_groups(
        final_groups.into_values().filter(|v| v.len() > 1),
        twin_groups.into_values().filter(|v| v.len() > 1),
    );
    let (groups, reclaimable) = write_groups(db, &comps, &info)?;

    let mut p = progress.lock().unwrap();
    p.hashed = cands.len();
    p.groups = groups;
    p.reclaimable = reclaimable;
    let out = p.clone();
    drop(p);
    on_progress(&out);
    Ok(out)
}

/// 4단계 후보를 고른다 — 이름·픽셀 크기가 같은 JPEG 이 둘 이상이고 그중 크기가 다른 것이 있는
/// 무리에서, 크기 차가 [`TWIN_SLACK`] 안인 짝이 있는 파일만.
fn twin_candidates(db: &Db) -> Result<Vec<Twin>> {
    let rows: Vec<(Twin, String, i64, i64)> = db.read(|c| {
        let mut st = c.prepare(
            "WITH b AS (SELECT name, width, height FROM files
                        WHERE trashed_at IS NULL AND kind = 0 AND width IS NOT NULL
                          AND ext IN ('jpg','jpeg')
                        GROUP BY name, width, height HAVING COUNT(DISTINCT size) > 1)
             SELECT fi.id, fo.rel_path, fi.name, fi.size, fo.volume_uuid, fi.image_hash, fi.full_hash,
                    fo.area, fi.taken_at, fi.width, fi.height, fi.culling_flag
             FROM files fi JOIN folders fo ON fo.id = fi.folder_id
             JOIN b ON b.name = fi.name AND b.width = fi.width AND b.height = fi.height
             WHERE fi.trashed_at IS NULL AND fi.kind = 0 AND fi.ext IN ('jpg','jpeg')
             ORDER BY fi.name, fi.width, fi.height, fi.size",
        )?;
        let it = st.query_map([], |r| {
            let dir: String = r.get(1)?;
            let name: String = r.get(2)?;
            Ok((
                Twin {
                    id: r.get(0)?,
                    path: PathBuf::from(crate::media::cache::rel_path(&dir, &name)),
                    volume_uuid: r.get(4)?,
                    image: r.get(5)?,
                    info: Info { size: r.get(3)?, area: r.get(7)?, taken_at: r.get(8)?, full: r.get(6)?, flag: r.get(11)? },
                },
                name,
                r.get::<_, i64>(9)?,
                r.get::<_, i64>(10)?,
            ))
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    // 같은 (이름, 가로, 세로) 무리 안에서 크기가 다르면서 가까운 짝이 있는 것만
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let key = |r: &(Twin, String, i64, i64)| (r.1.clone(), r.2, r.3);
        let k = key(&rows[i]);
        let mut j = i;
        while j < rows.len() && key(&rows[j]) == k {
            j += 1;
        }
        let bucket = &rows[i..j];
        let sizes: Vec<i64> = bucket.iter().map(|r| r.0.info.size).collect();
        for (n, r) in bucket.iter().enumerate() {
            let near = sizes
                .iter()
                .enumerate()
                .any(|(m, s)| m != n && *s != sizes[n] && (*s - sizes[n]).abs() <= TWIN_SLACK);
            if near {
                out.push(Twin {
                    id: r.0.id,
                    path: r.0.path.clone(),
                    volume_uuid: r.0.volume_uuid.clone(),
                    image: r.0.image.clone(),
                    info: r.0.info.clone(),
                });
            }
        }
        i = j;
    }
    Ok(out)
}

/// 두 종류의 무리를 합친다 — 한 파일이 «바이트 같음» 무리와 «그림 같음» 무리 양쪽에 있으면
/// 그 둘은 한 그룹이다 (union-find)
fn merge_groups(
    byte_groups: impl Iterator<Item = Vec<i64>>,
    image_groups: impl Iterator<Item = Vec<i64>>,
) -> Vec<Vec<i64>> {
    let mut parent: HashMap<i64, i64> = HashMap::new();
    fn find(parent: &mut HashMap<i64, i64>, x: i64) -> i64 {
        let p = *parent.entry(x).or_insert(x);
        if p == x {
            return x;
        }
        let root = find(parent, p);
        parent.insert(x, root);
        root
    }
    for g in byte_groups.chain(image_groups) {
        for w in g.windows(2) {
            let (a, b) = (find(&mut parent, w[0]), find(&mut parent, w[1]));
            if a != b {
                parent.insert(a, b);
            }
        }
    }
    let ids: Vec<i64> = parent.keys().copied().collect();
    let mut comps: HashMap<i64, Vec<i64>> = HashMap::new();
    for id in ids {
        let r = find(&mut parent, id);
        comps.entry(r).or_default().push(id);
    }
    let mut out: Vec<Vec<i64>> = comps.into_values().collect();
    for c in &mut out {
        c.sort_unstable();
    }
    out.sort_unstable();
    out
}

/// 그룹을 기록한다 — 이전 «완전 중복» 결과는 지운다. (그룹 수, 확보 가능 바이트)
fn write_groups(db: &Db, comps: &[Vec<i64>], info: &HashMap<i64, Info>) -> Result<(usize, i64)> {
    let mut reclaimable = 0i64;
    db.transaction(|tx| {
        // 이전 결과를 지운다 — 같은 종류를 두 번 쌓지 않게
        tx.execute("DELETE FROM groups WHERE kind = 0", [])?;
        let mut ins_g = tx.prepare(
            "INSERT INTO groups(kind, reason, size_bytes, state, created_at)
             VALUES(0, ?1, ?2, ?3, strftime('%s','now'))",
        )?;
        let mut ins_m =
            tx.prepare("INSERT INTO group_members(group_id, file_id, is_best) VALUES(?1,?2,?3)")?;
        for ids in comps {
            // 가장 이른 촬영일을 기본 유지본으로 제안한다.
            // 원본이 사본보다 먼저 찍혔을 가능성이 높다.
            // 정리된 자리(내사진·공용)에 있는 사본이 먼저다 — 옛 백업 디스크와
            // 운영 디스크 사이 중복에서 «올라간 쪽을 남기고 옛것을 버린다»가 되게.
            // 같은 자리끼리면 가장 이른 촬영일.
            let mut best = ids[0];
            let mut best_key = (i32::MAX, i32::MAX, i64::MAX, i64::MAX);
            let mut total = 0i64;
            let mut fulls: Vec<Option<&str>> = Vec::with_capacity(ids.len());
            let mut flags: Vec<i32> = Vec::with_capacity(ids.len());
            for id in ids {
                let i = info.get(id);
                let (area, t, flag) =
                    i.map(|c| (c.area, c.taken_at, c.flag))
                        .unwrap_or((i32::MAX, i64::MAX, 0));
                total += i.map(|c| c.size).unwrap_or(0);
                fulls.push(i.and_then(|c| c.full.as_deref()));
                flags.push(flag);
                // 폴더 비교가 먼저 «남김»을 붙였으면 그쪽이 대표 — 아니면 정착 구역 → 이른 촬영일.
                // 남김이 대표가 아니면 ★와 표시가 어긋나 보이고(실측 2,161무리), 확정이
                // 앞선 결정을 뒤집는다 (2026-08-31 동영상 쌍 지적)
                // 같은 자리·같은 시각이면 id 로 — 돌릴 때마다 대표가 바뀌지 않게
                let key = (
                    if flag == 1 { 0 } else { 1 },
                    if area == 1 || area == 2 { 0 } else { 1 },
                    t,
                    *id,
                );
                if key < best_key {
                    best_key = key;
                    best = *id;
                }
            }
            // 앞선 표시로 이미 결정된 무리(남김 하나 + 나머지 전부 제외)는 닫아서 만든다 —
            // «미결»로 두면 ✕ 붙은 쌍이 또 나와 «둘 다 제외인가»가 된다
            let decided =
                flags.iter().all(|f| *f != 0) && flags.iter().filter(|f| **f == 1).count() == 1;
            let state = if decided { 1 } else { 0 };
            // 바이트까지 다 같으면 «완전 중복», 그림만 같은 것이 섞였으면 «메타데이터만 다름»
            let all_same_bytes = fulls[0].is_some() && fulls.iter().all(|f| *f == fulls[0]);
            let reason = if all_same_bytes {
                "완전 중복"
            } else {
                "메타데이터만 다름"
            };
            // 한 장만 남기므로 나머지 크기만큼 확보된다
            let gain = total - info.get(&best).map(|c| c.size).unwrap_or(0);
            reclaimable += gain;
            ins_g.execute(rusqlite::params![reason, gain, state])?;
            let gid = tx.last_insert_rowid();
            for id in ids {
                ins_m.execute(rusqlite::params![gid, id, (*id == best) as i32])?;
            }
        }
        Ok(())
    })?;
    Ok((comps.len(), reclaimable))
}

#[cfg(test)]
mod tests;
