//! 촬영일 감사·교정. 감사는 읽기만 하고, 쓰기는 성공 항목별 복구 자료를 남긴다.

use crate::db::conn::{Db, DbError, IoContext, Result};
use crate::media::{exif, taken_at};
use crate::ops::trash::Outcome;
use chrono::Utc;
use filetime::FileTime;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditTarget {
    #[serde(default)]
    pub ids: Vec<i64>,
    pub library_id: Option<i64>,
    pub rel_path: Option<String>,
    #[serde(default = "yes")]
    pub recursive: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditItem {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub current_at: i64,
    pub current_source: i32,
    pub proposed_at: Option<i64>,
    pub proposed_source: Option<String>,
    pub evidence: String,
    pub interpretation: String,
    pub write_scope: String,
    pub auto_selected: bool,
    pub existing_exif: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub id: i64,
    pub taken_at: i64,
    #[serde(default)]
    pub manual: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ManifestItem {
    pub id: i64,
    pub path: String,
    pub before_sha256: String,
    pub after_sha256: String,
    pub before_taken_at: i64,
    pub written_at: i64,
    pub rescan_at: i64,
    pub rescan_source: i32,
    pub write_scope: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CaptureOutcome {
    pub batch_id: i64,
    pub corrected: usize,
    pub failed: usize,
    pub first_error: Option<String>,
    pub failed_ids: Vec<i64>,
    pub manifest: Vec<ManifestItem>,
}

#[derive(Debug, Clone)]
struct Item {
    id: i64,
    name: String,
    ext: String,
    kind: i32,
    taken_at: i64,
    source: i32,
    library_id: i64,
    volume_uuid: String,
    vol_rel: String,
    lib_rel: String,
}

fn validate_rel(rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(DbError::Invalid(
            "라이브러리 밖의 경로는 감사할 수 없습니다".into(),
        ));
    }
    Ok(())
}

fn load(db: &Db, target: &AuditTarget) -> Result<Vec<Item>> {
    let (where_sql, args): (String, Vec<rusqlite::types::Value>) = if !target.ids.is_empty() {
        let marks = std::iter::repeat_n("?", target.ids.len())
            .collect::<Vec<_>>()
            .join(",");
        (
            format!("fi.id IN ({marks})"),
            target.ids.iter().copied().map(Into::into).collect(),
        )
    } else {
        let library_id = target
            .library_id
            .ok_or_else(|| DbError::Invalid("감사할 사진이나 폴더를 정하세요".into()))?;
        let rel = target.rel_path.clone().unwrap_or_default();
        validate_rel(&rel)?;
        if rel.is_empty() {
            ("fo.library_id = ?".into(), vec![library_id.into()])
        } else if target.recursive {
            (
                "fo.library_id = ? AND (CASE WHEN l.rel_path='' THEN fo.rel_path ELSE substr(fo.rel_path,length(l.rel_path)+2) END = ? OR CASE WHEN l.rel_path='' THEN fo.rel_path ELSE substr(fo.rel_path,length(l.rel_path)+2) END LIKE ? || '/%')".into(),
                vec![library_id.into(), rel.clone().into(), rel.into()],
            )
        } else {
            (
                "fo.library_id = ? AND CASE WHEN l.rel_path='' THEN fo.rel_path ELSE substr(fo.rel_path,length(l.rel_path)+2) END = ?".into(),
                vec![library_id.into(), rel.into()],
            )
        }
    };
    let sql = format!(
        "SELECT fi.id,fi.name,COALESCE(fi.ext,''),fi.kind,fi.taken_at,fi.taken_at_source,
                fo.library_id,fo.volume_uuid,
                fo.rel_path || CASE WHEN fo.rel_path='' THEN '' ELSE '/' END || fi.name,
                CASE WHEN l.rel_path='' THEN fo.rel_path ELSE substr(fo.rel_path,length(l.rel_path)+2) END ||
                  CASE WHEN (CASE WHEN l.rel_path='' THEN fo.rel_path ELSE substr(fo.rel_path,length(l.rel_path)+2) END)='' THEN '' ELSE '/' END || fi.name
         FROM files fi JOIN folders fo ON fo.id=fi.folder_id JOIN libraries l ON l.id=fo.library_id
         WHERE fi.trashed_at IS NULL AND {where_sql} ORDER BY fi.id"
    );
    db.read(|c| {
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| {
            Ok(Item {
                id: r.get(0)?,
                name: r.get(1)?,
                ext: r.get(2)?,
                kind: r.get(3)?,
                taken_at: r.get(4)?,
                source: r.get(5)?,
                library_id: r.get(6)?,
                volume_uuid: r.get(7)?,
                vol_rel: r.get(8)?,
                lib_rel: r.get(9)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
}

fn one(db: &Db, id: i64) -> Result<Item> {
    load(
        db,
        &AuditTarget {
            ids: vec![id],
            library_id: None,
            rel_path: None,
            recursive: true,
        },
    )?
    .into_iter()
    .next()
    .ok_or_else(|| DbError::Invalid("사진을 찾을 수 없습니다".into()))
}

fn absolute(it: &Item) -> Result<PathBuf> {
    let mount = crate::db::volumes::find_mount(&it.volume_uuid)
        .ok_or_else(|| DbError::Invalid("디스크가 연결되어 있지 않습니다".into()))?;
    let p = mount.join(&it.vol_rel);
    if !p.is_file() {
        return Err(DbError::Invalid(format!(
            "파일을 찾을 수 없습니다: {}",
            it.lib_rel
        )));
    }
    Ok(p)
}

fn is_jpeg(it: &Item) -> bool {
    matches!(it.ext.as_str(), "jpg" | "jpeg") && it.kind == 0
}

pub fn audit(db: &Db, target: &AuditTarget) -> Result<Vec<AuditItem>> {
    load(db, target)?
        .into_iter()
        .map(|it| {
            let path = absolute(&it)?;
            let embedded = if it.kind == 1 {
                crate::media::video::probe(&path).taken_at
            } else {
                exif::read(&path).and_then(|m| m.taken_at)
            };
            let filename = taken_at::from_filename(&it.name);
            let (proposed, source, evidence, interpretation) = if let Some(t) = embedded {
                (
                    Some(t),
                    Some("embedded".into()),
                    "파일 내부 촬영일".into(),
                    "시간대 정보가 없는 지역 wall-clock으로 해석".into(),
                )
            } else if let Some(t) = filename {
                (
                    Some(t),
                    Some("filename".into()),
                    format!("파일명 {}", it.name),
                    "파일명의 날짜/시각을 지역 wall-clock으로 해석".into(),
                )
            } else {
                (
                    None,
                    None,
                    "고신뢰 촬영일 단서 없음".into(),
                    "수동 날짜·시각이 필요".into(),
                )
            };
            let auto_selected = embedded.is_none() && proposed.is_some();
            let write_scope = if is_jpeg(&it) {
                "JPEG EXIF 3필드 + mtime"
            } else {
                "mtime + Photo Desk 보정값 (파일 내부 메타데이터는 변경하지 않음)"
            }
            .into();
            Ok(AuditItem {
                id: it.id,
                name: it.name,
                path: it.lib_rel,
                current_at: it.taken_at,
                current_source: it.source,
                proposed_at: proposed,
                proposed_source: source,
                evidence,
                interpretation,
                write_scope,
                auto_selected,
                existing_exif: embedded.is_some(),
            })
        })
        .collect()
}

fn sha256(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn times(meta: &std::fs::Metadata) -> (FileTime, FileTime) {
    (
        FileTime::from_last_access_time(meta),
        FileTime::from_last_modification_time(meta),
    )
}

fn backup_path(db: &Db, it: &Item, batch_id: i64) -> Result<(PathBuf, String)> {
    let lib = crate::db::libraries::get(db, it.library_id)?
        .and_then(|l| l.dir)
        .ok_or_else(|| DbError::Invalid("디스크가 연결되어 있지 않습니다".into()))?;
    let safe = it.name.replace(['/', '\\', ':'], "-");
    let full = lib
        .join(".acut")
        .join("capture-date-backups")
        .join(batch_id.to_string())
        .join(format!("{}-{safe}", it.id));
    let mount = crate::db::volumes::find_mount(&it.volume_uuid)
        .ok_or_else(|| DbError::Invalid("디스크가 연결되어 있지 않습니다".into()))?;
    let rel = full
        .strip_prefix(&mount)
        .map_err(|_| DbError::Invalid("백업 경로가 볼륨 밖입니다".into()))?
        .to_string_lossy()
        .into_owned();
    Ok((full, rel))
}

fn refresh_values(path: &Path, wanted: i64, embedded: bool) -> Result<(i64, i32, u64, i64)> {
    let meta = std::fs::metadata(path).io_context("촬영일 파일 정보를 읽다가 실패했습니다")?;
    let mtime = FileTime::from_last_modification_time(&meta).unix_seconds();
    let (resolved, source) = if embedded {
        let read = exif::read(path)
            .and_then(|m| m.taken_at)
            .ok_or_else(|| DbError::Invalid("쓴 EXIF를 다시 읽지 못했습니다".into()))?;
        (read, taken_at::Source::Exif as i32)
    } else {
        (wanted, taken_at::Source::Manual as i32)
    };
    Ok((resolved, source, meta.len(), mtime))
}

fn restore_backup(
    backup: &Path,
    target: &Path,
    expected: &str,
    atime: FileTime,
    mtime: FileTime,
) -> Result<()> {
    let temp = crate::ops::trash::free_path(target.with_file_name(format!(
        ".{}.capture-restore.tmp",
        target.file_name().unwrap_or_default().to_string_lossy()
    )));
    std::fs::copy(backup, &temp).io_context("촬영일 백업을 임시 파일로 복사하다가 실패했습니다")?;
    if sha256(&temp).io_context("복원할 촬영일 백업의 해시를 읽다가 실패했습니다")? != expected
    {
        let _ = std::fs::remove_file(&temp);
        return Err(DbError::Invalid(
            "촬영일 백업의 SHA-256이 원본 기록과 다릅니다".into(),
        ));
    }
    std::fs::File::open(&temp)
        .io_context("복원할 임시 파일을 열다가 실패했습니다")?
        .sync_all()
        .io_context("복원할 임시 파일을 디스크에 기록하다가 실패했습니다")?;
    std::fs::rename(&temp, target)
        .io_context("촬영일 백업을 원본 위치로 복원하다가 실패했습니다")?;
    filetime::set_file_times(target, atime, mtime)
        .io_context("복원한 파일의 시각을 되돌리다가 실패했습니다")?;
    Ok(())
}

pub fn apply(db: &Db, changes: &[Change], label: &str) -> Result<CaptureOutcome> {
    if changes.is_empty() {
        return Ok(CaptureOutcome {
            first_error: Some("교정할 사진이 없습니다".into()),
            ..Default::default()
        });
    }
    let batch_id = super::open_batch(db, "capture_date", label)?;
    let mut out = CaptureOutcome {
        batch_id,
        ..Default::default()
    };
    let now = Utc::now().timestamp();
    for change in changes {
        let result = (|| -> Result<ManifestItem> {
            if !taken_at::is_plausible(change.taken_at, now) {
                return Err(DbError::Invalid("허용 범위 밖의 촬영일입니다".into()));
            }
            let it = one(db, change.id)?;
            let path = absolute(&it)?;
            let embedded_before = if it.kind == 1 {
                crate::media::video::probe(&path).taken_at
            } else {
                exif::read(&path).and_then(|m| m.taken_at)
            };
            if !change.manual {
                if embedded_before.is_some() {
                    return Err(DbError::Invalid(
                        "유효한 파일 내부 촬영일은 자동으로 덮어쓰지 않습니다".into(),
                    ));
                }
                let proposed = taken_at::from_filename(&it.name).ok_or_else(|| {
                    DbError::Invalid("자동 교정할 고신뢰 파일명 단서가 없습니다".into())
                })?;
                if proposed != change.taken_at {
                    return Err(DbError::Invalid(
                        "감사 뒤 파일 정보가 바뀌었습니다. 다시 감사하세요".into(),
                    ));
                }
            }
            let meta = std::fs::metadata(&path)
                .io_context("교정할 사진의 파일 정보를 읽다가 실패했습니다")?;
            let (atime, mtime) = times(&meta);
            let old_override: Option<i64> = db.read(|c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT taken_at FROM capture_date_overrides WHERE file_id=?1",
                    [it.id],
                    |r| r.get(0),
                )
                .optional()
            })?;
            let before_hash =
                sha256(&path).io_context("교정 전 사진의 해시를 읽다가 실패했습니다")?;
            let (backup_vol, backup_rel) = if is_jpeg(&it) {
                let (backup, rel) = backup_path(db, &it, batch_id)?;
                if let Some(parent) = backup.parent() {
                    std::fs::create_dir_all(parent)
                        .io_context("촬영일 백업 폴더를 만들다가 실패했습니다")?;
                }
                std::fs::copy(&path, &backup)
                    .io_context("촬영일 교정 전 원본을 백업하다가 실패했습니다")?;
                std::fs::File::open(&backup)
                    .io_context("촬영일 백업 파일을 열다가 실패했습니다")?
                    .sync_all()
                    .io_context("촬영일 백업을 디스크에 기록하다가 실패했습니다")?;
                if sha256(&backup).io_context("촬영일 백업의 해시를 확인하다가 실패했습니다")?
                    != before_hash
                {
                    let _ = std::fs::remove_file(&backup);
                    return Err(DbError::Invalid("촬영일 백업 검증에 실패했습니다".into()));
                }
                (Some(it.volume_uuid.clone()), Some(rel))
            } else {
                (None, None)
            };

            let changed = (|| -> Result<ManifestItem> {
                if is_jpeg(&it) {
                    crate::media::exif_write::write_capture_time(&path, change.taken_at)
                        .map_err(|e| DbError::Invalid(e.to_string()))?;
                }
                filetime::set_file_times(
                    &path,
                    atime,
                    FileTime::from_unix_time(change.taken_at, 0),
                )
                .io_context("교정한 파일의 수정 시각을 쓰다가 실패했습니다")?;
                let embedded = is_jpeg(&it);
                let (rescan_at, rescan_source, size, modified_at) =
                    refresh_values(&path, change.taken_at, embedded)?;
                let after_hash =
                    sha256(&path).io_context("교정 후 사진의 해시를 읽다가 실패했습니다")?;
                let scope = if embedded {
                    "jpeg-exif+mtime"
                } else {
                    "mtime+desk-override"
                };
                db.transaction(|tx| {
                    if embedded {
                        tx.execute(
                            "DELETE FROM capture_date_overrides WHERE file_id=?1",
                            [it.id],
                        )?;
                    } else {
                        tx.execute(
                            "INSERT INTO capture_date_overrides(file_id,taken_at) VALUES(?1,?2)
                             ON CONFLICT(file_id) DO UPDATE SET taken_at=excluded.taken_at,updated_at=strftime('%s','now')",
                            rusqlite::params![it.id, change.taken_at],
                        )?;
                    }
                    tx.execute(
                        "UPDATE files SET taken_at=?2,taken_at_source=?3,size=?4,modified_at=?5,
                          quick_hash=NULL,full_hash=NULL,image_hash=NULL,phash=NULL,psig=NULL,scanned_at=strftime('%s','now') WHERE id=?1",
                        rusqlite::params![it.id, rescan_at, rescan_source, size as i64, modified_at],
                    )?;
                    tx.execute("DELETE FROM thumbs WHERE file_id=?1", [it.id])?;
                    tx.execute(
                        "INSERT INTO journal(batch_id,file_id,op,from_vol,from_path,to_vol,to_path,ok)
                         VALUES(?1,?2,'capture_date',?3,?4,?3,?4,1)",
                        rusqlite::params![batch_id,it.id,it.volume_uuid,it.vol_rel],
                    )?;
                    tx.execute(
                        "INSERT INTO capture_date_journal(batch_id,file_id,backup_vol,backup_path,
                         old_atime_sec,old_atime_nsec,old_mtime_sec,old_mtime_nsec,old_taken_at,old_source,
                         old_override,new_taken_at,write_scope,before_sha256,after_sha256)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                        rusqlite::params![batch_id,it.id,backup_vol,backup_rel,atime.unix_seconds(),atime.nanoseconds(),
                            mtime.unix_seconds(),mtime.nanoseconds(),it.taken_at,it.source,old_override,change.taken_at,scope,
                            before_hash,after_hash],
                    )?;
                    Ok(())
                })?;
                Ok(ManifestItem {
                    id: it.id,
                    path: it.lib_rel.clone(),
                    before_sha256: before_hash.clone(),
                    after_sha256: after_hash,
                    before_taken_at: it.taken_at,
                    written_at: change.taken_at,
                    rescan_at,
                    rescan_source,
                    write_scope: scope.into(),
                })
            })();
            match changed {
                Ok(manifest) => Ok(manifest),
                Err(error) => {
                    let rollback = if let Some(ref rel) = backup_rel {
                        crate::db::volumes::find_mount(&it.volume_uuid)
                            .ok_or_else(|| {
                                DbError::Invalid("백업 볼륨이 연결되어 있지 않습니다".into())
                            })
                            .and_then(|mount| {
                                restore_backup(&mount.join(rel), &path, &before_hash, atime, mtime)
                            })
                    } else {
                        filetime::set_file_times(&path, atime, mtime)
                            .io_context("교정 실패 뒤 파일 시각을 복구하다가 실패했습니다")
                    };
                    match rollback {
                        Ok(()) => Err(error),
                        Err(rollback) => Err(DbError::Invalid(format!(
                            "촬영일 교정 실패: {error}; 원본 복구도 실패: {rollback}"
                        ))),
                    }
                }
            }
        })();
        match result {
            Ok(m) => {
                out.corrected += 1;
                out.manifest.push(m);
            }
            Err(e) => {
                out.failed += 1;
                out.failed_ids.push(change.id);
                out.first_error.get_or_insert(e.to_string());
                let msg = e.to_string();
                let _ = super::record(
                    db,
                    batch_id,
                    "capture_date",
                    change.id,
                    "",
                    "",
                    None,
                    Err(&msg),
                );
            }
        }
    }
    super::close_batch(db, batch_id, out.corrected)?;
    Ok(out)
}

pub fn undo(db: &Db, batch_id: i64) -> Result<Outcome> {
    #[derive(Debug)]
    struct Row {
        file_id: i64,
        backup_vol: Option<String>,
        backup_path: Option<String>,
        at_s: i64,
        at_n: u32,
        mt_s: i64,
        mt_n: u32,
        old_at: i64,
        old_src: i32,
        old_override: Option<i64>,
        before_sha256: Option<String>,
        after_sha256: Option<String>,
    }
    let rows: Vec<Row> = db.read(|c| {
        let mut st=c.prepare("SELECT file_id,backup_vol,backup_path,old_atime_sec,old_atime_nsec,old_mtime_sec,old_mtime_nsec,old_taken_at,old_source,old_override,before_sha256,after_sha256 FROM capture_date_journal WHERE batch_id=?1 AND undone_at IS NULL ORDER BY file_id DESC")?;
        let it=st.query_map([batch_id],|r| Ok(Row{file_id:r.get(0)?,backup_vol:r.get(1)?,backup_path:r.get(2)?,at_s:r.get(3)?,at_n:r.get::<_,i64>(4)? as u32,mt_s:r.get(5)?,mt_n:r.get::<_,i64>(6)? as u32,old_at:r.get(7)?,old_src:r.get(8)?,old_override:r.get(9)?,before_sha256:r.get(10)?,after_sha256:r.get(11)?}))?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut out = Outcome {
        batch_id,
        ..Default::default()
    };
    for row in rows {
        let result = (|| -> Result<()> {
            let it = one(db, row.file_id)?;
            let target = absolute(&it)?;
            let expected_after = row.after_sha256.as_deref().ok_or_else(|| {
                DbError::Invalid(
                    "0.9.0에서 만든 촬영일 작업은 변경 후 해시가 없어 안전하게 되돌릴 수 없습니다"
                        .into(),
                )
            })?;
            if sha256(&target).io_context("되돌릴 교정 파일의 해시를 읽다가 실패했습니다")?
                != expected_after
            {
                return Err(DbError::Invalid(
                    "교정 뒤 파일 내용이 바뀌어 원본으로 덮어쓰지 않았습니다".into(),
                ));
            }
            let corrected_times = times(
                &std::fs::metadata(&target)
                    .io_context("되돌릴 교정 파일 정보를 읽다가 실패했습니다")?,
            );
            let mut staged_corrected = None;
            if let (Some(vol), Some(rel)) = (row.backup_vol.as_deref(), row.backup_path.as_deref())
            {
                let mount = crate::db::volumes::find_mount(vol)
                    .ok_or_else(|| DbError::Invalid("백업 볼륨이 연결되어 있지 않습니다".into()))?;
                let backup = mount.join(rel);
                if !backup.is_file() {
                    return Err(DbError::Invalid("촬영일 백업 파일이 없습니다".into()));
                }
                let expected_before = row.before_sha256.as_deref().ok_or_else(|| {
                    DbError::Invalid("원본 SHA-256 기록이 없어 되돌릴 수 없습니다".into())
                })?;
                if sha256(&backup).io_context("되돌릴 촬영일 백업의 해시를 읽다가 실패했습니다")?
                    != expected_before
                {
                    return Err(DbError::Invalid(
                        "촬영일 백업의 SHA-256이 원본 기록과 다릅니다".into(),
                    ));
                }
                let corrected = crate::ops::trash::free_path(target.with_file_name(format!(
                    ".{}.capture-undo-{batch_id}.tmp",
                    target.file_name().unwrap_or_default().to_string_lossy()
                )));
                std::fs::rename(&target, &corrected)
                    .io_context("교정 파일을 임시 위치로 옮기다가 실패했습니다")?;
                if let Err(error) = restore_backup(
                    &backup,
                    &target,
                    expected_before,
                    FileTime::from_unix_time(row.at_s, row.at_n),
                    FileTime::from_unix_time(row.mt_s, row.mt_n),
                ) {
                    let _ = std::fs::rename(&corrected, &target);
                    return Err(error);
                }
                staged_corrected = Some(corrected);
            } else {
                filetime::set_file_times(
                    &target,
                    FileTime::from_unix_time(row.at_s, row.at_n),
                    FileTime::from_unix_time(row.mt_s, row.mt_n),
                )
                .io_context("원본 파일의 시각을 복원하다가 실패했습니다")?;
            }
            let updated = db.transaction(|tx| {
                match row.old_override {
                    Some(t)=>{tx.execute("INSERT INTO capture_date_overrides(file_id,taken_at) VALUES(?1,?2) ON CONFLICT(file_id) DO UPDATE SET taken_at=excluded.taken_at",rusqlite::params![row.file_id,t])?;}
                    None=>{tx.execute("DELETE FROM capture_date_overrides WHERE file_id=?1",[row.file_id])?;}
                }
                let meta=std::fs::metadata(&target).map_err(|e|rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                tx.execute("UPDATE files SET taken_at=?2,taken_at_source=?3,size=?4,modified_at=?5,quick_hash=NULL,full_hash=NULL,image_hash=NULL,phash=NULL,psig=NULL WHERE id=?1",rusqlite::params![row.file_id,row.old_at,row.old_src,meta.len() as i64,row.mt_s])?;
                tx.execute("DELETE FROM thumbs WHERE file_id=?1",[row.file_id])?;
                tx.execute(
                    "UPDATE capture_date_journal SET undone_at=strftime('%s','now') WHERE batch_id=?1 AND file_id=?2",
                    rusqlite::params![batch_id,row.file_id],
                )?;
                Ok(())
            });
            if let Err(error) = updated {
                let rollback = if let Some(corrected) = staged_corrected.as_ref() {
                    let _ = std::fs::remove_file(&target);
                    std::fs::rename(corrected, &target).and_then(|()| {
                        filetime::set_file_times(&target, corrected_times.0, corrected_times.1)
                    })
                } else {
                    filetime::set_file_times(&target, corrected_times.0, corrected_times.1)
                };
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(DbError::Invalid(format!(
                        "되돌리기 DB 갱신 실패: {error}; 교정 파일 복구도 실패: {rollback}"
                    ))),
                };
            }
            if let Some(corrected) = staged_corrected {
                if let Err(error) = std::fs::remove_file(&corrected) {
                    log::warn!(
                        "촬영일 되돌리기 임시 파일을 지우지 못했습니다 {}: {error}",
                        corrected.display()
                    );
                }
            }
            if let (Some(vol), Some(rel)) = (row.backup_vol.as_deref(), row.backup_path.as_deref())
            {
                if let Some(mount) = crate::db::volumes::find_mount(vol) {
                    let backup = mount.join(rel);
                    let _ = std::fs::remove_file(&backup);
                    if let Some(parent) = backup.parent() {
                        let _ = std::fs::remove_dir(parent);
                    }
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => out.moved += 1,
            Err(e) => {
                out.failed += 1;
                out.failed_ids.push(row.file_id);
                out.first_error.get_or_insert(e.to_string());
            }
        }
    }
    let remaining: i64 = db.read(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM capture_date_journal WHERE batch_id=?1 AND undone_at IS NULL",
            [batch_id],
            |r| r.get(0),
        )
    })?;
    if remaining == 0 {
        crate::ops::undo::mark_undone(db, batch_id)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
