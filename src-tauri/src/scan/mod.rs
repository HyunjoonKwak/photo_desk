//! 스캐너 — 폴더를 훑어 DB에 넣는다.
//!
//! 설계상 지켜야 할 것들
//!   - **NFC 정규화**: macOS 파일시스템은 한글을 NFD(자모 분리)로 준다. NAS(ext4)는
//!     NFC다. 정규화하지 않으면 같은 파일이 다른 이름으로 보여 대조가 어긋난다.
//!     실제로 이 프로젝트에서 중복률이 64.9%로 잘못 나온 적이 있다(실제 76.7%).
//!   - **볼륨 UUID + 상대경로**: 절대경로를 저장하지 않는다.
//!   - **배치 삽입**: 낱개 INSERT는 매번 fsync가 걸린다. 트랜잭션으로 묶는다.
//!   - **증분**: 크기와 수정시각이 그대로면 다시 읽지 않는다.

use crate::db::conn::Db;
use crate::media::{exif, taken_at};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

pub mod kinds;
pub mod thumbs;
pub mod watch;

pub use kinds::Kind;

/// 스캔 진행 상황. UI로 흘려보낸다.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Progress {
    pub found: usize,
    pub inserted: usize,
    pub updated: usize,
    pub skipped: usize,
    pub failed: usize,
    /// 이번 훑기에서 디스크에 없어 지운 파일 행 수 (Finder 에서 지운 것)
    pub removed: usize,
    /// 그래서 비어 지운 폴더 행 수
    pub folders_removed: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("볼륨을 인식할 수 없습니다: {0}")]
    Volume(#[from] crate::db::volumes::VolumeError),
    #[error("데이터베이스 오류: {0}")]
    Db(#[from] crate::db::conn::DbError),
    #[error("SQLite 오류: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("스캔할 폴더가 없습니다: {0}")]
    NotADirectory(PathBuf),
}

type Result<T> = std::result::Result<T, ScanError>;

/// 파일시스템에서 발견한 파일 하나 (아직 DB에 넣기 전).
#[derive(Debug)]
struct Found {
    rel_dir: String,
    name: String,
    size: u64,
    kind: Kind,
    mtime: Option<i64>,
    birthtime: Option<i64>,
    inode: u64,
    full_path: PathBuf,
}

/// 문자열을 NFC로 정규화한다. 경로·파일명은 **반드시** 이걸 거쳐야 한다.
pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// 라이브러리 하나를 스캔해 DB에 반영한다.
///
/// `library_id`는 등록된 라이브러리, `root`는 그 실제 경로다. 찾아낸 폴더는 전부
/// 이 라이브러리에 속하게 된다 — 썸네일 캐시와 원본 경로를 나중에 이걸로 푼다.
/// `area`는 이 폴더가 어느 영역인지 (0 작업대 · 1 내사진 · 2 공용 · 3 기타).
pub fn scan_folder(
    db: &Db,
    library_id: i64,
    root: impl AsRef<Path>,
    area: i32,
    on_progress: impl Fn(&Progress) + Sync + Send,
) -> Result<Progress> {
    let root = root.as_ref();
    if !root.is_dir() {
        return Err(ScanError::NotADirectory(root.to_path_buf()));
    }

    // 볼륨을 먼저 등록한다. 이 UUID가 모든 경로의 기준이 된다.
    let vol = crate::db::volumes::describe(root)?;
    db.write(|c| {
        c.execute(
            "INSERT INTO volumes(uuid,name,last_mount_path,role,total_bytes,free_bytes,is_online,last_seen_at)
             VALUES(?1,?2,?3,'library',?4,?5,1,strftime('%s','now'))
             ON CONFLICT(uuid) DO UPDATE SET
               name=excluded.name, last_mount_path=excluded.last_mount_path,
               total_bytes=excluded.total_bytes, free_bytes=excluded.free_bytes,
               is_online=1, last_seen_at=excluded.last_seen_at",
            rusqlite::params![
                vol.uuid,
                vol.name,
                vol.mount_path.to_string_lossy(),
                vol.total_bytes as i64,
                vol.free_bytes as i64
            ],
        )
    })?;

    // 폴더를 훑는 동안에도 알린다. 8만 장을 다 세고 나서야 첫 알림을 보내면
    // 그때까지 화면이 «아무 반응 없음»이다 — exFAT USB면 수십 초다.
    on_progress(&Progress::default());
    let mut last_found = std::time::Instant::now();
    let found = walk(root, &vol.mount_path, |n| {
        if last_found.elapsed() >= std::time::Duration::from_millis(200) {
            last_found = std::time::Instant::now();
            on_progress(&Progress {
                found: n,
                ..Default::default()
            });
        }
    });
    let progress = Arc::new(std::sync::Mutex::new(Progress {
        found: found.len(),
        ..Default::default()
    }));
    on_progress(&progress.lock().unwrap().clone());

    // 이미 아는 파일은 건너뛴다 — (상대경로, 이름) → (크기, 수정시각)
    let known = load_known(db, library_id)?;

    let last_emit = std::sync::Mutex::new(std::time::Instant::now());
    let now = now_secs();

    // 무거운 부분(EXIF 읽기)만 병렬로. DB 쓰기는 뒤에서 한 번에 한다.
    let rows: Vec<_> = found
        .par_iter()
        .filter_map(|f| {
            let key = (f.rel_dir.clone(), f.name.clone());
            if let Some(&(sz, mt)) = known.get(&key) {
                if sz == f.size as i64 && mt == f.mtime.unwrap_or(0) {
                    progress.lock().unwrap().skipped += 1;
                    return None; // 바뀐 게 없다
                }
            }
            // 영상은 ImageIO가 못 읽는다. Spotlight에서 촬영 시각·해상도를 가져온다.
            // probe는 한 번만 부른다 — 두 번 부르면 스캔이 두 배로 느려진다.
            let (m, duration_ms) = if f.kind == Kind::Video {
                let v = crate::media::video::probe(&f.full_path);
                (
                    exif::Meta {
                        taken_at: v.taken_at,
                        width: v.width.map(|x| x as u32),
                        height: v.height.map(|x| x as u32),
                        ..Default::default()
                    },
                    // 0은 "읽어 봤지만 없더라"는 뜻이다. NULL은 "아직 안 읽었다".
                    // 이 구분이 없으면 Spotlight가 모르는 영상을 스캔할 때마다
                    // 다시 뒤진다 (실측 1,357개 × 26개/초 ≈ 52초).
                    Some(v.duration_ms.unwrap_or(0)),
                )
            } else {
                (exif::read(&f.full_path).unwrap_or_default(), None)
            };
            // 영상의 taken_at_source도 0(exif)으로 남는다. 파일 안에 박힌
            // 메타데이터라는 뜻이라 의미가 같다 — 출처가 EXIF가 아니라 컨테이너일 뿐.
            let (ts, src) = if kinds::classify(&f.name) == Some(Kind::Video) {
                // 영상은 단서 중 가장 이른 것 — 컨테이너 시각은 재인코딩 날로 바뀌기 일쑤다
                let folder = f.rel_dir.rsplit('/').next().unwrap_or("");
                taken_at::resolve_video(m.taken_at, &f.name, folder, f.mtime, f.birthtime, now)
            } else {
                taken_at::resolve(m.taken_at, &f.name, f.mtime, f.birthtime, now)
            };

            // 스캔 쪽도 시간 기준으로. 500장마다면 숫자가 껑충 뛴다.
            let due = {
                let mut l = last_emit.lock().unwrap();
                if l.elapsed() >= std::time::Duration::from_millis(50) {
                    *l = std::time::Instant::now();
                    true
                } else {
                    false
                }
            };
            if due {
                on_progress(&progress.lock().unwrap().clone());
            }
            Some((f, m, ts, src, duration_ms))
        })
        .collect();

    // 폴더를 먼저 만들고(부모→자식 순서), 그 다음 파일을 넣는다.
    let mut dirs: Vec<&String> = rows.iter().map(|(f, _, _, _, _)| &f.rel_dir).collect();
    dirs.sort();
    dirs.dedup();

    db.transaction(|tx| {
        for d in &dirs {
            let name = Path::new(d)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| d.to_string());
            tx.execute(
                "INSERT INTO folders(volume_uuid,library_id,rel_path,name,area,scanned_at)
                 VALUES(?1,?2,?3,?4,?5,strftime('%s','now'))
                 ON CONFLICT(volume_uuid,rel_path) DO UPDATE SET
                   library_id=excluded.library_id, scanned_at=excluded.scanned_at",
                rusqlite::params![vol.uuid, library_id, d, name, area],
            )?;
        }

        let mut ins = tx.prepare(
            "INSERT INTO files(folder_id,name,ext,size,kind,taken_at,taken_at_source,
                created_at,modified_at,width,height,orientation,duration_ms,
                cam_make,cam_model,lens,iso,aperture,shutter,focal_mm,
                gps_lat,gps_lon,gps_alt,inode,scanned_at)
             VALUES((SELECT id FROM folders WHERE volume_uuid=?1 AND rel_path=?2),
                ?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?25,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,
                strftime('%s','now'))
             ON CONFLICT(folder_id,name) DO UPDATE SET
                quick_hash=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.quick_hash END,
                full_hash=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.full_hash END,
                image_hash=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.image_hash END,
                phash=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.phash END,
                psig=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.psig END,
                size=excluded.size, taken_at=excluded.taken_at,
                taken_at_source=excluded.taken_at_source,
                ext=excluded.ext, kind=excluded.kind,
                created_at=excluded.created_at,
                modified_at=excluded.modified_at, width=excluded.width,
                height=excluded.height, orientation=excluded.orientation,
                duration_ms=excluded.duration_ms,
                cam_make=excluded.cam_make, cam_model=excluded.cam_model,
                lens=excluded.lens, iso=excluded.iso,
                aperture=excluded.aperture, shutter=excluded.shutter,
                focal_mm=excluded.focal_mm,
                geo_name=CASE WHEN files.gps_lat IS excluded.gps_lat
                    AND files.gps_lon IS excluded.gps_lon THEN files.geo_name END,
                geo_country=CASE WHEN files.gps_lat IS excluded.gps_lat
                    AND files.gps_lon IS excluded.gps_lon THEN files.geo_country END,
                geo_admin1=CASE WHEN files.gps_lat IS excluded.gps_lat
                    AND files.gps_lon IS excluded.gps_lon THEN files.geo_admin1 END,
                geo_admin2=CASE WHEN files.gps_lat IS excluded.gps_lat
                    AND files.gps_lon IS excluded.gps_lon THEN files.geo_admin2 END,
                gps_lat=excluded.gps_lat, gps_lon=excluded.gps_lon,
                gps_alt=excluded.gps_alt, inode=excluded.inode,
                sharpness=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.sharpness END,
                exposure=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.exposure END,
                embedding=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.embedding END,
                faces_at=CASE WHEN files.size=excluded.size
                    AND files.modified_at IS excluded.modified_at THEN files.faces_at END,
                scanned_at=excluded.scanned_at",
        )?;

        for (f, m, ts, src, duration_ms) in &rows {
            let r = ins.execute(rusqlite::params![
                vol.uuid,
                f.rel_dir,
                f.name,
                Path::new(&f.name)
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase()),
                f.size as i64,
                f.kind as i32,
                ts,
                *src as i32,
                f.birthtime,
                f.mtime,
                m.width,
                m.height,
                m.orientation,
                m.cam_make,
                m.cam_model,
                m.lens,
                m.iso,
                m.aperture,
                m.shutter,
                m.focal_mm,
                m.gps_lat,
                m.gps_lon,
                m.gps_alt,
                f.inode as i64,
                duration_ms,
            ]);
            let mut p = progress.lock().unwrap();
            match r {
                Ok(1) => p.inserted += 1,
                Ok(_) => p.updated += 1,
                Err(_) => p.failed += 1,
            }
        }
        Ok(())
    })?;

    // 파일 내부에 촬영일을 쓸 수 없는 형식의 명시적 수동 교정은 재스캔 뒤에도
    // 유지한다. 자동 파일명 판독보다 사용자의 명시적 결정을 우선한다.
    db.write(|c| {
        c.execute(
            "UPDATE files SET
                 taken_at = (SELECT o.taken_at FROM capture_date_overrides o WHERE o.file_id=files.id),
                 taken_at_source = 4
             WHERE id IN (SELECT file_id FROM capture_date_overrides)
               AND folder_id IN (SELECT id FROM folders WHERE library_id=?1)",
            [library_id],
        )
    })?;

    // 디스크에서 사라진 것의 행을 뺀다 — 훑은 뿌리 아래에서 이번에 못 본 파일.
    // 예전엔 전체 다시 스캔도 이걸 안 해 Finder 에서 지운 폴더 269개가 «없는 폴더»로 남았다 (2026-08-30)
    {
        let root_rel = root
            .strip_prefix(&vol.mount_path)
            .map(|p| nfc(&p.to_string_lossy()))
            .unwrap_or_default();
        let (removed, folders_removed) = prune_gone(db, library_id, &root_rel, &found)?;
        let mut p = progress.lock().unwrap();
        p.removed = removed;
        p.folders_removed = folders_removed;
    }

    // 폴더별 파일 수를 갱신한다 (사이드바에서 쓴다).
    db.write(|c| {
        c.execute(
            "UPDATE folders SET file_count =
               (SELECT COUNT(*) FROM files
                 WHERE files.folder_id = folders.id AND files.trashed_at IS NULL)
             WHERE library_id = ?1",
            [library_id],
        )
    })?;
    db.write(|c| {
        c.execute(
            "UPDATE libraries SET scanned_at = strftime('%s','now') WHERE id = ?1",
            [library_id],
        )
    })?;

    // 새로 들어오거나 좌표가 바뀐 사진에, 이미 아는 지명을 곧바로 붙인다.
    //
    // 여기서 하는 이유: 전체 스캔·가져오기·폴더 감시·EXIF 재읽기가 모두 이
    // 함수를 지난다. 부르는 쪽마다 따로 붙이면 한 곳은 반드시 빠진다. 그리고
    // 이 자리는 `scan-done` 을 알리기 **전**이라, 화면이 새로 고칠 때는 이미
    // 이름이 붙어 있다 (2026-09-01).
    //
    // 바뀐 것이 없으면 건너뛴다 — 감시는 폴더가 바뀔 때마다 이 함수를 부른다.
    let out = progress.lock().unwrap().clone();
    if out.inserted > 0 || out.updated > 0 {
        match crate::geo::propagate_library(db, library_id) {
            Ok(n) if n > 0 => log::info!("스캔 뒤 지명 캐시 적용 — {n}장"),
            Ok(_) => {}
            // 지명은 곁들이일 뿐이다 — 실패해도 스캔 자체는 성공이다
            Err(e) => log::warn!("스캔 뒤 지명 캐시 적용 실패: {e}"),
        }
    }
    on_progress(&out);
    Ok(out)
}

/// 훑은 뿌리(`root_rel`, 볼륨 기준) 아래에서 이번에 못 본 파일의 행과, 그래서 빈 폴더 행을 지운다.
///
/// 휴지통에 든 것(`trashed_at`)은 원래 자리에 없는 게 정상이라 두고, 휴지통 파일이 가리키는
/// 폴더 행도 남긴다(FK CASCADE). 안전장치: 훑은 것이 하나도 없거나 절반 넘게 사라졌으면
/// 마운트가 빠졌거나 잘못 붙은 것으로 보고 손대지 않는다.
fn prune_gone(db: &Db, library_id: i64, root_rel: &str, found: &[Found]) -> Result<(usize, usize)> {
    let seen: std::collections::HashSet<(&str, &str)> = found
        .iter()
        .map(|f| (f.rel_dir.as_str(), f.name.as_str()))
        .collect();
    let rows: Vec<(i64, String, String)> = db.read(|c| {
        let mut st = c.prepare(
            "SELECT fi.id, fo.rel_path, fi.name FROM files fi JOIN folders fo ON fo.id = fi.folder_id
             WHERE fo.library_id = ?1 AND fi.trashed_at IS NULL",
        )?;
        let it = st.query_map([library_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let under = |dir: &str| {
        root_rel.is_empty() || dir == root_rel || dir.starts_with(&format!("{root_rel}/"))
    };
    let scoped: Vec<&(i64, String, String)> = rows.iter().filter(|(_, d, _)| under(d)).collect();
    let gone: Vec<i64> = scoped
        .iter()
        .filter(|(_, d, n)| !seen.contains(&(d.as_str(), n.as_str())))
        .map(|(id, _, _)| *id)
        .collect();
    if gone.is_empty() {
        return Ok((0, 0));
    }
    if found.is_empty() || (scoped.len() >= 100 && gone.len() * 2 > scoped.len()) {
        log::warn!(
            "훑은 뿌리 «{root_rel}» 에서 {}개 중 {}개가 안 보인다 — 디스크가 빠진 것으로 보고 지우지 않는다",
            scoped.len(),
            gone.len()
        );
        return Ok((0, 0));
    }
    let folders_removed = db.transaction(|tx| {
        let mut del = tx.prepare("DELETE FROM files WHERE id = ?1")?;
        for id in &gone {
            del.execute([id])?;
        }
        drop(del);
        tx.execute(
            "DELETE FROM folders WHERE library_id = ?1
               AND NOT EXISTS (SELECT 1 FROM files WHERE files.folder_id = folders.id)",
            [library_id],
        )
    })?;
    log::info!(
        "사라진 파일 {}개·빈 폴더 {}개 행을 지웠다 (뿌리 «{root_rel}»)",
        gone.len(),
        folders_removed
    );
    Ok((gone.len(), folders_removed))
}

/// 한 폴더 안에서 **디스크에 없어진** 파일의 행을 지운다. 지운 수를 돌려준다.
///
/// 스캔은 있는 것만 넣는다. 파인더에서 지운 것은 여기서 뺀다. 휴지통에 든
/// 것(`trashed_at`)은 원래 자리에 없는 게 정상이라 건드리지 않는다.
/// 썸네일 파일은 두고 행만 지운다 — 같은 파일이 돌아오면 캐시 키가 같아 그대로 쓴다.
pub fn prune_missing(db: &Db, mount: &Path, library_id: i64, rel_dir: &str) -> Result<usize> {
    let rows: Vec<(i64, String)> = db.read(|c| {
        let mut st = c.prepare(
            "SELECT fi.id, fo.rel_path || CASE WHEN fo.rel_path = '' THEN '' ELSE '/' END || fi.name
               FROM files fi JOIN folders fo ON fo.id = fi.folder_id
              WHERE fo.library_id = ?1 AND fo.rel_path = ?2 AND fi.trashed_at IS NULL",
        )?;
        let it = st.query_map(rusqlite::params![library_id, rel_dir], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let gone: Vec<i64> = rows
        .into_iter()
        .filter(|(_, rel)| !mount.join(rel).exists())
        .map(|(id, _)| id)
        .collect();
    if gone.is_empty() {
        return Ok(0);
    }
    db.transaction(|tx| {
        let mut del = tx.prepare("DELETE FROM files WHERE id = ?1")?;
        for id in &gone {
            del.execute([id])?;
        }
        tx.execute(
            "UPDATE folders SET file_count =
               (SELECT COUNT(*) FROM files
                 WHERE files.folder_id = folders.id AND files.trashed_at IS NULL)
             WHERE library_id = ?1",
            [library_id],
        )?;
        Ok(())
    })?;
    Ok(gone.len())
}

/// 시험용 — 폴더를 라이브러리로 등록하고 곧바로 스캔한다.
///
/// 실제 흐름에서는 등록(`library_add`)과 스캔(`scan_start`)이 나뉘어 있지만,
/// 시험에서는 항상 붙어 다닌다.
#[cfg(test)]
pub fn scan_test(
    db: &Db,
    root: impl AsRef<Path>,
    area: i32,
    on_progress: impl Fn(&Progress) + Sync + Send,
) -> Result<Progress> {
    let root = root.as_ref();
    let lib = crate::db::libraries::add(db, root, area)
        .unwrap_or_else(|e| panic!("라이브러리 등록: {e}"));
    scan_folder(db, lib.id, root, area, on_progress)
}

/// 이미 DB에 있는 파일들의 (크기, 수정시각). 증분 스캔의 재료다.
fn load_known(
    db: &Db,
    library_id: i64,
) -> Result<std::collections::HashMap<(String, String), (i64, i64)>> {
    let map = db.read(|c| {
        let mut st = c.prepare(
            // 영상인데 duration_ms가 NULL이면 아직 메타데이터를 안 읽은 것이다.
            // 크기·수정시각이 그대로여도 한 번은 다시 봐야 한다.
            "SELECT fo.rel_path, fi.name, fi.size, COALESCE(fi.modified_at,0)
             FROM files fi JOIN folders fo ON fo.id = fi.folder_id
             WHERE fo.library_id = ?1
               AND NOT (fi.kind = 1 AND fi.duration_ms IS NULL)",
        )?;
        let rows = st.query_map([library_id], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                (r.get::<_, i64>(2)?, r.get::<_, i64>(3)?),
            ))
        })?;
        let mut m = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            m.insert(k, v);
        }
        Ok(m)
    })?;
    Ok(map)
}

/// 폴더를 재귀로 훑는다. 심볼릭 링크는 따라가지 않는다(순환 방지).
/// 폴더를 훑는다. `on_found`는 찾은 수가 늘 때마다 (호출자가 솎아 쓴다).
fn walk(root: &Path, mount: &Path, mut on_found: impl FnMut(usize)) -> Vec<Found> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let name_raw = entry.file_name();
            let name = nfc(&name_raw.to_string_lossy());
            if ft.is_dir() {
                if kinds::is_skipped_dir(&name) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let Some(kind) = kinds::classify(&name) else {
                continue;
            };
            let Ok(md) = entry.metadata() else { continue };
            let rel_dir = dir
                .strip_prefix(mount)
                .ok()
                .map(|p| nfc(&p.to_string_lossy()))
                .unwrap_or_default();
            out.push(Found {
                rel_dir,
                name,
                size: md.len(),
                kind,
                mtime: unix(md.modified().ok()),
                birthtime: unix(md.created().ok()),
                inode: {
                    use std::os::unix::fs::MetadataExt;
                    md.ino()
                },
                full_path: path,
            });
            on_found(out.len());
        }
    }
    out
}

fn unix(t: Option<std::time::SystemTime>) -> Option<i64> {
    t?.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod real;
