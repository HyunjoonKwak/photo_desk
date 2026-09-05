use super::online::{ask_with_retry, judge, Answer, Verdict, ONLINE_NONE, ONLINE_OK};
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// 이미 이름이 붙은 사진을 다시 쓸 것인가 — B3 덮어쓰기 규칙의 유일한 갈림길이다.
///
/// 규칙은 세 줄이다: 온라인 결과는 오프라인 결과를 덮는다. 오프라인 결과는
/// 이름이 없는 곳에만 쓴다. 어느 쪽도 사람이 손댄 값을 덮지 않는다(아직 없다).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overwrite {
    /// 이름이 아직 없는 사진에만
    OnlyEmpty,
    /// 이 자리의 사진 전부 — 더 정밀한 결과로 바꿔 붙인다
    All,
}

impl Overwrite {
    fn filter(self) -> &'static str {
        match self {
            Overwrite::OnlyEmpty => "AND geo_country IS NULL",
            Overwrite::All => "",
        }
    }
}

/// 어느 경로로 채우나
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 서버 없이 — 내장 자료로 곧바로 채운다
    Offline,
    /// 서버에 물어 정밀하게 — 오프라인으로 채운 자리도 다시 묻는다
    Online,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Progress {
    /// 이름이 필요한 격자 수
    pub total: usize,
    /// 여기까지 처리한 격자 수 (캐시로 채운 것 포함)
    pub done: usize,
    /// 새로 물어본 수
    pub asked: usize,
    /// 이름을 붙인 사진 수
    pub files: usize,
    /// 그 자리에 이름이 없어 다시 묻지 않기로 한 격자 수
    pub empty: usize,
    /// 멈췄으면 그 사유. 비어 있으면 «끝까지 다 했다»는 뜻이다.
    pub stopped: Option<String>,
    /// 사용자가 멈춘 것인가 — 서버 탓과 달리 경고로 보일 일이 아니다
    pub cancelled: bool,
}

/// 캐시 한 줄을 쓰고 그 자리의 사진에 전파한다 — **한 트랜잭션**으로.
///
/// 중간에 앱이 꺼져도 places 와 files 가 어긋나지 않는다. 실패하면 둘 다 그대로다.
/// `INSERT OR REPLACE` 가 아니라 `ON CONFLICT DO UPDATE` 를 쓴다 — 행을 지웠다
/// 다시 만들면 나중에 붙일 외래 키·트리거가 조용히 깨진다 (2026-09-01 리뷰).
#[allow(clippy::too_many_arguments)]
pub(super) fn write_place(
    db: &Db,
    cell_key: &str,
    place: &Place,
    status: &str,
    source: &str,
    precision: &str,
    distance_km: Option<f64>,
    dataset_version: Option<&str>,
    provider: Option<&str>,
    gps: &str,
    overwrite: Overwrite,
    // 온라인 조회 결과도 함께 남길 때. 오프라인 경로는 None 을 준다 —
    // 오프라인이 값을 채웠다고 해서 «서버에 물어봤다»가 되지는 않는다.
    online: Option<&str>,
) -> Result<usize> {
    let name = place.name();
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,
                                distance_km,dataset_version,provider,resolved_at,
                                online_outcome,online_provider,online_checked_at,at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,strftime('%s','now'),
                    ?12,?13,CASE WHEN ?12 IS NULL THEN NULL ELSE strftime('%s','now') END,
                    strftime('%s','now'))
             ON CONFLICT(cell) DO UPDATE SET
               country=excluded.country, admin1=excluded.admin1, admin2=excluded.admin2,
               name=excluded.name, status=excluded.status, source=excluded.source,
               precision=excluded.precision, distance_km=excluded.distance_km,
               dataset_version=excluded.dataset_version, provider=excluded.provider,
               resolved_at=excluded.resolved_at,
               -- 조회 결과는 물어봤을 때만 덮는다. 오프라인이 값을 채워도
               -- 앞서 서버가 답한 이력은 지우지 않는다.
               online_outcome=COALESCE(excluded.online_outcome, places.online_outcome),
               online_provider=COALESCE(excluded.online_provider, places.online_provider),
               online_checked_at=COALESCE(excluded.online_checked_at, places.online_checked_at)",
            rusqlite::params![
                cell_key,
                &place.country,
                &place.admin1,
                &place.admin2,
                &name,
                status,
                source,
                precision,
                distance_km,
                dataset_version,
                provider,
                online,
                if online.is_some() { provider } else { None }
            ],
        )?;
        if place.country.is_none() {
            return Ok(0);
        }
        // 이 자리의 사진에 전파
        let n = tx.execute(
            &format!(
                "UPDATE files SET geo_country = ?2, geo_admin1 = ?3, geo_admin2 = ?4,
                        geo_name = COALESCE(?4, ?3, ?2)
                 WHERE {cell} = ?1 AND {gps} {only}",
                cell = cell_sql("gps_lat", "gps_lon"),
                only = overwrite.filter()
            ),
            rusqlite::params![cell_key, &place.country, &place.admin1, &place.admin2],
        )?;
        Ok(n)
    })
}

/// 이 자리에 이미 몇 단계까지 붙어 있나 — 새 답이 그보다 얕으면 덮지 않는다
pub(super) fn current_depth(db: &Db, cell_key: &str) -> Result<u8> {
    let place: Option<Place> = db.read(|c| {
        c.query_row(
            "SELECT country, admin1, admin2 FROM places WHERE cell = ?1 AND status = 'ok'",
            [cell_key],
            |r| {
                Ok(Place {
                    country: r.get(0)?,
                    admin1: r.get(1)?,
                    admin2: r.get(2)?,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
    })?;
    Ok(place.map(|p| p.depth()).unwrap_or(0))
}

/// 값은 건드리지 않고 «이 서버에 물어봤고 결과가 이랬다»만 남긴다.
///
/// 이 기록이 대상 고르기의 열쇠다. 없으면 값이 그대로라는 이유로 같은 좌표가
/// 다음 실행에서 또 뽑혀 같은 서버에 되풀이해 묻는다 — 보강이 끝나지 않는다.
/// 서버가 바뀌면 `online_provider` 가 달라 다시 물을 수 있다.
fn record_online(db: &Db, cell_key: &str, outcome: &str, provider: Option<&str>) -> Result<()> {
    db.write(|c| {
        c.execute(
            "INSERT INTO places(cell,status,source,online_outcome,online_provider,online_checked_at,at)
             VALUES(?1,?2,?3,?4,?5,strftime('%s','now'),strftime('%s','now'))
             ON CONFLICT(cell) DO UPDATE SET
               online_outcome=excluded.online_outcome,
               online_provider=excluded.online_provider,
               online_checked_at=excluded.online_checked_at",
            rusqlite::params![cell_key, UNRESOLVED, SRC_ONLINE, outcome, provider],
        )
    })?;
    Ok(())
}

/// «그 자리에 이름이 없다»를 캐시에 못 박는다 — **이미 이름이 있으면 그대로 둔다.**
///
/// 이름을 지우는 일은 이 앱 어디에도 없어야 한다. 서버가 못 찾은 것과 그 자리에
/// 이름이 없는 것은 다르다. 이미 붙은 이름이 있으면 그것을 남기고 «물어봤다»만
/// 적는다 — 그래야 그 자리가 다음 실행에서 또 뽑히지 않는다. 돌려주는 값은
/// 실제로 못 박았는지 여부다.
pub(super) fn settle_empty(
    db: &Db,
    cell_key: &str,
    status: &str,
    source: &str,
    provider: Option<&str>,
) -> Result<bool> {
    db.transaction(|tx| {
        let named: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM places
                            WHERE cell = ?1 AND country IS NOT NULL AND trim(country) <> '')",
            [cell_key],
            |r| r.get(0),
        )?;
        if named {
            // 값과 출처는 그대로 — 바뀐 것은 «이 서버가 못 찾았다»는 사실뿐이다
            tx.execute(
                "UPDATE places
                    SET online_outcome = ?2, online_provider = ?3,
                        online_checked_at = strftime('%s','now')
                  WHERE cell = ?1",
                rusqlite::params![cell_key, ONLINE_NONE, provider],
            )?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO places(cell,status,source,precision,provider,resolved_at,
                                online_outcome,online_provider,online_checked_at,at)
             VALUES(?1,?2,?3,?4,?5,strftime('%s','now'),?6,?5,strftime('%s','now'),strftime('%s','now'))
             ON CONFLICT(cell) DO UPDATE SET
               status=excluded.status, source=excluded.source, precision=excluded.precision,
               provider=excluded.provider, resolved_at=excluded.resolved_at,
               online_outcome=excluded.online_outcome, online_provider=excluded.online_provider,
               online_checked_at=excluded.online_checked_at",
            rusqlite::params![cell_key, status, source, PREC_REMOTE, provider, ONLINE_NONE],
        )?;
        Ok(true)
    })
}

/// 이미 캐시에 있는 값을 그 자리의 사진에 붙인다 (네트워크 없이)
fn propagate(
    db: &Db,
    cell_key: &str,
    place: &Place,
    gps: &str,
    overwrite: Overwrite,
) -> Result<usize> {
    db.write(|c| {
        c.execute(
            &format!(
                "UPDATE files SET geo_country = ?2, geo_admin1 = ?3, geo_admin2 = ?4,
                        geo_name = COALESCE(?4, ?3, ?2)
                 WHERE {cell} = ?1 AND {gps} {only}",
                cell = cell_sql("gps_lat", "gps_lon"),
                only = overwrite.filter()
            ),
            rusqlite::params![cell_key, &place.country, &place.admin1, &place.admin2],
        )
    })
}

/// 처리할 자리와 그 자리의 대표 좌표.
///
/// 격자는 «같은 곳을 두 번 묻지 않기» 위한 열쇠일 뿐이고, 대표 좌표는 그 칸에
/// **실제로 있는 사진들의 중앙값**이다 — 칸 정중앙을 쓰면 경계·해안·섬에서 옆
/// 동네가 붙는다 (2026-09-01 리뷰).
///
/// 무엇을 대상으로 삼는가가 두 경로의 유일한 차이다:
/// - 오프라인: 아직 아무 판정도 없는 자리. 온라인이 이미 정한 것은 건드리지 않는다.
/// - 온라인: 이름이 없는 자리와, 오프라인이 채워 둔 자리(정밀 보강). 서버가
///   «이름 없음»으로 확정한 자리(none)는 어느 쪽도 다시 묻지 않는다.
pub(super) fn targets(
    db: &Db,
    mode: Mode,
    gps: &str,
    provider: Option<&str>,
) -> Result<Vec<(String, f64, f64)>> {
    let cell_expr = cell_sql("gps_lat", "gps_lon");
    let want = match mode {
        // 아직 오프라인이 손대지 않았고, 이름이 없는 사진이 있는 자리.
        //
        // 온라인이 먼저 돌아 충돌·불완전 응답을 만나면 값 없는 `unresolved` 행이
        // 생긴다. 그때 «판정이 아예 없는 자리»만 고르면 그 좌표는 오프라인으로도
        // 영영 복구되지 않는다 — 온라인이 실패했다는 이유로 내장 자료마저 막히는
        // 셈이다. 그래서 **오프라인이 스스로 포기한 자리만** 건너뛴다.
        // 이미 판정된 자리의 미전파는 propagate_all 이 따로 되메운다.
        Mode::Offline => {
            "(p.status IS NULL
              OR (p.status = 'unresolved' AND COALESCE(p.source,'') <> 'offline_geonames'))
             AND t.unnamed > 0"
        }
        // 이름이 없거나, 오프라인 결과라 더 정밀해질 수 있는 자리.
        //
        // 고르는 열쇠는 값이 아니라 **«이 서버에 물어봤나»** 다. 값만 보면 서버가
        // 못 찾았거나 얕게 답한 자리가 «값이 그대로»라는 이유로 매번 다시 뽑혀
        // 같은 서버에 되풀이해 묻는다 — 보강이 끝나지 않는다.
        //
        // 그래서 `status='none'` 을 여기서 걸러내지 않는다. «이름이 없다»는 것은
        // 그 서버의 답일 뿐이고, 다른 서버는 알 수도 있다. 서버가 바뀌면
        // online_provider 가 달라 다시 물어본다. 옛 판에서 넘어온 행은
        // online_provider 가 비어 있어 새 서버에서 꼭 한 번 다시 물어본다.
        Mode::Online => {
            "(t.unnamed > 0 OR p.source = 'offline_geonames')
             AND (p.online_outcome IS NULL
                  OR (?1 IS NOT NULL
                      AND (p.online_provider IS NULL OR p.online_provider <> ?1)))"
        }
    };
    db.read(|c| {
        let mut st = c.prepare(&format!(
            "WITH pts AS (
               SELECT {cell_expr} AS cell, gps_lat AS la, gps_lon AS lo,
                      ROW_NUMBER() OVER (PARTITION BY {cell_expr} ORDER BY gps_lat, gps_lon) AS rn,
                      COUNT(*) OVER (PARTITION BY {cell_expr}) AS n,
                      SUM(geo_country IS NULL) OVER (PARTITION BY {cell_expr}) AS unnamed
                 FROM files
                WHERE {gps} AND trashed_at IS NULL
             ),
             t AS (SELECT * FROM pts WHERE rn = (n + 1) / 2)
             SELECT t.cell, t.la, t.lo
               FROM t LEFT JOIN places p ON p.cell = t.cell
              WHERE {want}
              ORDER BY t.cell",
        ))?;
        // 오프라인 조건에는 «어느 서버에 물었나»가 없다 — 자리 표시자를 쓰지 않는
        // 질의에 값을 넘기면 SQLite 가 개수 불일치로 거절한다
        fn row(r: &rusqlite::Row) -> rusqlite::Result<(String, f64, f64)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }
        match mode {
            Mode::Offline => st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>(),
            Mode::Online => st
                .query_map(rusqlite::params![provider], row)?
                .collect::<rusqlite::Result<Vec<_>>>(),
        }
    })
}

/// 이미 쓸 수 있는 이름이 캐시에 있는데 아직 붙지 않은 사진이 있는 자리 수.
///
/// 스캔으로 새 사진이 들어오거나 좌표가 바뀌어 지명이 지워지면 이 자리가 생긴다.
/// 서버도 내장 자료도 필요 없다 — 가진 값을 옮겨 붙이기만 하면 된다. 화면이 이
/// 수를 세지 않으면 «처리할 곳 0» 이라 단추가 꺼지고, 사용자는 서버를 설정하지
/// 않는 한 새 사진에 이름을 붙일 길이 없어진다 (2026-09-01).
pub(super) fn cache_cells_left(db: &Db, gps: &str) -> Result<i64> {
    let cell_expr = cell_sql("gps_lat", "gps_lon");
    db.read(|c| {
        c.query_row(
            &format!(
                "SELECT COUNT(*) FROM (
                   SELECT {cell_expr} AS cell
                     FROM files
                    WHERE {gps} AND trashed_at IS NULL AND geo_country IS NULL
                    GROUP BY cell
                 ) t
                 JOIN places p ON p.cell = t.cell
                WHERE p.status = 'ok' AND p.country IS NOT NULL AND trim(p.country) <> ''",
            ),
            [],
            |r| r.get(0),
        )
    })
}

/// 이 서버를 가리키는 이름 — 조회 이력의 열쇠.
///
/// 호스트만으로는 모자란다. 같은 기계에서 포트나 경로만 다르게 띄운 두 서버는
/// 서로 다른 자료를 가질 수 있는데, 호스트만 보면 «같은 서버»로 여겨 한쪽이
/// 못 찾은 자리를 다른 쪽에 물어보지 않는다.
///
/// **물음표 뒤(query)와 조각(fragment)은 뺀다.** 자체 Nominatim 을 `?key=...` 로
/// 지키는 구성이 흔한데, 그 열쇠가 DB 에 남으면 안 된다. 대소문자와 끝 빗금도
/// 고르게 만들어 같은 서버를 다르게 세지 않는다.
pub(super) fn provider_of(endpoint: Option<&str>) -> Option<String> {
    let raw = endpoint?;
    validate_endpoint(raw).ok()?;
    let url = reqwest::Url::parse(raw).ok()?;
    let scheme = url.scheme().to_ascii_lowercase();
    let host = url.host_str()?.to_ascii_lowercase();
    // 그 scheme 의 기본 포트면 Url 이 None 을 준다 — 적지 않아야 같은 것이 같아진다
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    Some(format!("{scheme}://{host}{port}{path}"))
}

/// 캐시에 이미 있는 이름을 이름 없는 사진에 **한 번에** 붙인다.
///
/// 자리마다 UPDATE 를 돌리면 그때마다 파일 5만 행을 훑는다(실측 자리당 139ms,
/// 1,120곳에 156초). 격자 열쇠가 계산식이라 인덱스를 못 쓰기 때문이다. 그래서
/// 한 번만 훑고 places 를 PK 로 붙인다.
///
/// 이 걸음은 언제 돌려도 안전하고, 판정과 전파가 중간에 끊겨 어긋난 것도 여기서
/// 되메워진다 — 그래서 오프라인 채우기는 늘 이것으로 끝난다.
fn propagate_all(db: &Db, gps: &str) -> Result<usize> {
    propagate_scoped(db, gps, None)
}

/// 한 라이브러리 안에서만 붙인다 — 스캔 뒤에 쓴다.
///
/// 감시는 폴더가 바뀔 때마다 스캔을 부르므로, 그때마다 파일 표 전체를 훑으면
/// 라이브러리가 여럿일수록 헛일이 커진다.
pub fn propagate_library(db: &Db, library_id: i64) -> Result<usize> {
    propagate_scoped(db, &valid_gps_sql(), Some(library_id))
}

fn propagate_scoped(db: &Db, gps: &str, library_id: Option<i64>) -> Result<usize> {
    let cell = cell_sql("gps_lat", "gps_lon");
    let scope = if library_id.is_some() {
        "AND folder_id IN (SELECT id FROM folders WHERE library_id = ?1)"
    } else {
        ""
    };
    db.write(|c| {
        let sql = format!(
            "UPDATE files SET
                   geo_country = (SELECT p.country FROM places p WHERE p.cell = {cell}),
                   geo_admin1  = (SELECT p.admin1  FROM places p WHERE p.cell = {cell}),
                   geo_admin2  = (SELECT p.admin2  FROM places p WHERE p.cell = {cell}),
                   geo_name    = (SELECT p.name    FROM places p WHERE p.cell = {cell})
                 WHERE {gps} AND geo_country IS NULL
                   AND EXISTS(SELECT 1 FROM places p
                               WHERE p.cell = {cell} AND p.status = 'ok'
                                 AND p.country IS NOT NULL AND trim(p.country) <> '')
                   {scope}",
        );
        match library_id {
            Some(id) => c.execute(&sql, [id]),
            None => c.execute(&sql, []),
        }
    })
}

/// 이미 아는 이름을 아직 못 받은 사진에 붙인다 — 서버도 내장 자료도 필요 없다.
///
/// 스캔이 끝날 때마다 한 번 돈다. 새 사진이 이미 처리한 자리에 들어오는 일은
/// 흔한데, 그때마다 사용자가 «채우기»를 눌러야 한다면 그 사진은 대개 이름 없이
/// 남는다. 붙인 사진 수를 돌려준다.
pub fn propagate_cached(db: &Db) -> Result<usize> {
    propagate_all(db, &valid_gps_sql())
}

/// 서버 없이 채운다 — 내장한 도시·경계 자료로 곧바로 판정한다.
///
/// 망도, 설정도, 기다림도 없다. 결과는 «근사»로 표시되고 나중에 정밀 보강이
/// 덮어쓸 수 있다. 판정하지 못한 자리(바다 위 등)는 `unresolved` 로 남겨 두어
/// 온라인 경로가 다시 시도한다.
///
/// 판정과 전파를 나눈다: 자리를 다 판정해 캐시에 적은 뒤, 사진에는 마지막에
/// 한 번만 붙인다. 중간에 멈춰도 캐시는 남고, 다음 실행의 마지막 걸음이 전파를
/// 마저 한다.
pub fn fill_offline(
    db: &Db,
    cancel: &AtomicBool,
    limit: Option<usize>,
    on_progress: impl Fn(&Progress),
) -> Result<Progress> {
    let gps = valid_gps_sql();
    let todo = targets(db, Mode::Offline, &gps, None)?;
    let todo: Vec<_> = match limit {
        Some(n) => todo.into_iter().take(n).collect(),
        None => todo,
    };
    let version = offline::dataset_version();
    // 판정할 자리와 «가진 값을 옮겨 붙이기만 하면 되는» 자리를 함께 센다 —
    // 화면이 안내한 수와 실제로 하는 일이 같아야 한다
    let cached = cache_cells_left(db, &gps)? as usize;
    let mut p = Progress {
        total: todo.len() + cached,
        ..Default::default()
    };
    on_progress(&p);

    // 한 트랜잭션이 너무 길면 그동안 다른 작업이 DB 를 못 쓴다
    const CHUNK: usize = 500;
    for chunk in todo.chunks(CHUNK) {
        if cancel.load(Ordering::Relaxed) {
            p.stopped = Some("멈췄습니다".into());
            p.cancelled = true;
            break;
        }
        let judged: Vec<(&str, f64, f64, Option<resolve::Resolved>)> = chunk
            .iter()
            .map(|(k, lat, lon)| (k.as_str(), *lat, *lon, resolve::resolve(*lat, *lon)))
            .collect();
        let empty = judged.iter().filter(|(_, _, _, r)| r.is_none()).count();

        db.transaction(|tx| {
            for (cell_key, _, _, r) in &judged {
                let (place, status, precision, distance) = match r {
                    Some(r) => (r.place.clone(), OK, r.precision, r.distance_km),
                    None => (Place::default(), UNRESOLVED, PREC_APPROX, None),
                };
                tx.execute(
                    "INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,
                                        distance_km,dataset_version,resolved_at,at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,strftime('%s','now'),strftime('%s','now'))
                     ON CONFLICT(cell) DO UPDATE SET
                       country=excluded.country, admin1=excluded.admin1, admin2=excluded.admin2,
                       name=excluded.name, status=excluded.status, source=excluded.source,
                       precision=excluded.precision, distance_km=excluded.distance_km,
                       dataset_version=excluded.dataset_version, resolved_at=excluded.resolved_at",
                    rusqlite::params![
                        cell_key, &place.country, &place.admin1, &place.admin2, place.name(),
                        status, SRC_OFFLINE, precision, distance, version
                    ],
                )?;
            }
            Ok(())
        })?;

        p.done += judged.len();
        p.empty += empty;
        on_progress(&p);
    }

    // 사진에 붙이는 것은 마지막에 한 번 — 파일 표를 한 번만 훑는다.
    // 이 걸음이 캐시만 있으면 되는 자리까지 함께 메운다.
    p.files = propagate_all(db, &gps)?;
    if p.stopped.is_none() {
        p.done = p.total;
    }
    on_progress(&p);
    Ok(p)
}

/// 서버에 물어 정밀하게 채운다.
///
/// 이름이 없는 자리와, 오프라인이 채워 둔 자리(정밀 보강)를 대상으로 한다.
/// 캐시에 이미 **온라인** 결과가 있으면 묻지 않고 사진에만 붙인다 — 오프라인
/// 결과는 캐시로 치지 않는다(그것을 더 낫게 만드는 것이 이 경로의 일이다).
///
/// 서버가 잠깐 흔들리면 세 번까지 다시 묻고(1·2·4초, Retry-After 존중), 주소나
/// 권한이 틀린 답이면 그 자리에서 멈춘다 — 채운 것은 남고 다음에 이어서 한다.
pub fn fill(
    db: &Db,
    cancel: &AtomicBool,
    limit: Option<usize>,
    on_progress: impl Fn(&Progress),
) -> Result<Progress> {
    let gps = valid_gps_sql();
    // 서버가 없어도 기존 성공 캐시는 파일에 적용할 수 있다. 실제로 새 좌표를
    // 물어야 하는 순간에만 설정을 요구한다.
    let endpoint = endpoint_setting(db)?;
    let zoom: u8 = crate::db::settings::get(db, "geo.zoom")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);

    let provider = provider_of(endpoint.as_deref());
    let todo = targets(db, Mode::Online, &gps, provider.as_deref())?;

    let todo: Vec<_> = match limit {
        Some(n) => todo.into_iter().take(n).collect(),
        None => todo,
    };
    let mut p = Progress {
        total: todo.len(),
        ..Default::default()
    };
    on_progress(&p);
    if todo.is_empty() {
        return Ok(p);
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        // 허용된 주소가 공개 Nominatim으로 우회되지 않게 리다이렉트도 중단한다.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| crate::db::conn::DbError::Invalid(e.to_string()))?;

    for (cell_key, lat, lon) in todo {
        if cancel.load(Ordering::Relaxed) {
            p.stopped = Some("멈췄습니다".into());
            p.cancelled = true;
            break;
        }
        // 캐시부터 — 성공한 것만 쓴다
        // 캐시로 치는 것은 **온라인** 성공뿐이다. 오프라인 결과를 캐시로 삼으면
        // 정밀 보강이 영영 일어나지 않는다.
        let cached: Option<Place> = db.read(|c| {
            c.query_row(
                "SELECT country, admin1, admin2 FROM places
                  WHERE cell = ?1 AND status = ?2 AND source = ?3
                    AND country IS NOT NULL AND trim(country) <> ''",
                rusqlite::params![&cell_key, OK, SRC_ONLINE],
                |r| {
                    Ok(Place {
                        country: r.get(0)?,
                        admin1: r.get(1)?,
                        admin2: r.get(2)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })?;

        let place = match cached {
            Some(place) => place,
            None => {
                let endpoint = endpoint
                    .as_deref()
                    .map(validate_endpoint)
                    .transpose()?
                    .ok_or_else(|| crate::db::conn::DbError::Invalid(
                        "설정 › 탐색에서 자체 Nominatim 또는 배치 사용이 허용된 지명 서버를 먼저 입력해 주세요".into(),
                    ))?;
                std::thread::sleep(GAP); // 같은 서버에는 초당 하나
                p.asked += 1;
                // 고르는 열쇠와 적는 열쇠는 반드시 같은 함수에서 나와야 한다
                let host = provider_of(Some(endpoint.as_str()));
                match ask_with_retry(&client, endpoint.as_str(), lat, lon, zoom, cancel) {
                    Answer::Found(found) => {
                        // 내장 경계가 나라를 아는 자리에서는 경계가 이긴다 —
                        // 독도에 «일본»이 오는 답으로 정책이 뒤집히지 않게.
                        let known = boundary::country(lat, lon);
                        let admin1_ok = found
                            .place
                            .admin1
                            .as_deref()
                            .and_then(|a| boundary::admin1_matches(lat, lon, a));
                        let verdict = judge(
                            known.as_deref(),
                            found.cc.as_deref(),
                            admin1_ok,
                            found.place.depth(),
                            current_depth(db, &cell_key)?,
                        );
                        if verdict != Verdict::Accept {
                            log::info!(
                                "서버 답을 받아들이지 않습니다({}): {cell_key} — 경계 {known:?} · 답 {:?}",
                                verdict.outcome(), found.cc
                            );
                            // 값은 그대로 두고 «물어봤다»만 남긴다 — 같은 서버에 되풀이해 묻지 않게
                            record_online(db, &cell_key, verdict.outcome(), host.as_deref())?;
                            p.done += 1;
                            on_progress(&p);
                            continue;
                        }
                        // 캐시 갱신과 파일 전파를 한 트랜잭션에 둔다 — 중간에 앱이 꺼져도
                        // places 와 files 가 어긋나지 않는다 (2026-09-01 리뷰)
                        write_place(
                            db,
                            &cell_key,
                            &found.place,
                            OK,
                            SRC_ONLINE,
                            PREC_REMOTE,
                            None,
                            None,
                            host.as_deref(),
                            &gps,
                            Overwrite::All,
                            Some(ONLINE_OK),
                        )?;
                        found.place
                    }
                    Answer::Nothing => {
                        // 서버가 «없다»고 확정한 자리 — 못 박아 두고 다시 묻지 않는다.
                        //
                        // 단, **이미 이름이 붙은 자리는 지우지 않는다.** 새 서버가 못
                        // 찾았다는 것이 오프라인이 찾아 둔 이름이 틀렸다는 뜻은 아니다
                        // (2026-09-01 외부 검토).
                        if !settle_empty(db, &cell_key, NONE, SRC_ONLINE, host.as_deref())? {
                            log::info!("지명 없음이라 하지만 이미 붙은 이름이 있어 그대로 둡니다: {cell_key}");
                        }
                        p.empty += 1;
                        p.done += 1;
                        on_progress(&p);
                        continue;
                    }
                    Answer::Retryable { message, .. } => {
                        // 세 번을 다 쓰고도 안 됐다 — 채운 것은 남기고 멈춘다
                        log::warn!("지명 조회 중단 {cell_key}: {message}");
                        p.stopped = Some(format!("{message} — 잠시 뒤에 다시 해 주세요"));
                        break;
                    }
                    Answer::Fatal(e) => {
                        log::warn!("지명 조회 중단 {cell_key}: {e}");
                        p.stopped = Some(e);
                        break;
                    }
                }
            }
        };

        // 캐시 적중이면 파일 전파만 하면 된다
        if !place.is_empty() {
            p.files += propagate(db, &cell_key, &place, &gps, Overwrite::All)?;
        }
        p.done += 1;
        on_progress(&p);
    }
    Ok(p)
}

/// 얼마나 남았나 — 설정 화면이 «지명 채우기» 앞에 보여 준다.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    /// 쓸 수 있는 좌표가 있는 사진
    pub with_gps: i64,
    /// 그중 이름이 붙은 사진 (오프라인·온라인 합)
    pub named: i64,
    /// 이름이 붙었으나 온라인으로 더 정밀하게 만들 수 있는 사진 (오프라인 결과)
    pub approximate_files: i64,
    /// 온라인 정밀 결과가 붙은 사진
    pub precise_files: i64,
    /// 아직 이름이 없고 **처리할 수 있는** 사진 (캐시 적용 또는 조회 대상)
    pub pending_files: i64,
    /// 온라인 서버가 «이름 없음»으로 확정한 사진 — 더 할 일이 없다
    pub unavailable_files: i64,
    /// 처리할 자리 수
    pub cells_left: i64,
    /// 그중 오프라인으로 새로 판정할 자리 (스냅샷만 있으면 된다)
    pub offline_cells_left: i64,
    /// 이미 캐시에 이름이 있는데 아직 안 붙은 자리 — 옮겨 붙이기만 하면 된다
    pub cache_cells_left: i64,
    /// 그중 서버에 물어야만 하는 자리 (오프라인이 이미 포기한 곳)
    pub network_cells_left: i64,
    /// 서버에 물을 수 있는 자리 전부 — 못 채운 곳과 오프라인으로 채워 둔 곳(정밀 보강)
    pub online_cells_left: i64,
    /// 새 조회에 쓸 수 있는 비공개/허가된 서버가 설정됐나
    pub endpoint_ready: bool,
}

pub fn stats(db: &Db) -> Result<Stats> {
    let gps = valid_gps_sql();
    let endpoint = endpoint_setting(db)?;
    let endpoint_ready = endpoint
        .as_deref()
        .is_some_and(|s| validate_endpoint(s).is_ok());
    // 온라인으로 «물어볼 곳»은 어느 서버에 물을지에 달렸다 — 이미 이 서버가
    // 답한 자리는 세지 않아야 화면의 수가 0까지 줄어든다
    let provider = provider_of(endpoint.as_deref());
    let mut stats = db.read(|c| {
        // 52,000행을 먼저 1,143개 자리로 접고, places 는 PK 로 한 번만 붙인다.
        // 행마다 상관 서브쿼리를 돌리던 이전 방식은 실측 0.23초였다 (2026-09-01 리뷰)
        c.query_row(
            &format!(
                "WITH valid AS (
                   SELECT {cell} AS cell, geo_country
                     FROM files
                    WHERE {gps} AND trashed_at IS NULL
                 ),
                 by_cell AS (
                   SELECT cell,
                          COUNT(*) AS files,
                          SUM(geo_country IS NOT NULL) AS named
                     FROM valid GROUP BY cell
                 ),
                 joined AS (
                   SELECT b.cell, b.files, b.named,
                          p.status, p.source, p.precision, p.country,
                          p.online_outcome, p.online_provider
                     FROM by_cell b LEFT JOIN places p ON p.cell = b.cell
                 )
                 SELECT
                   SUM(files),
                   SUM(named),
                   -- 이름은 있으나 온라인으로 더 정밀해질 수 있는 것
                   SUM(CASE WHEN named > 0 AND source = '{offline}' THEN named ELSE 0 END),
                   SUM(CASE WHEN named > 0 AND source = '{online}' THEN named ELSE 0 END),
                   -- 아직 이름이 없고 처리할 수 있는 것 (none 만 제외)
                   SUM(CASE WHEN status IS NULL OR status <> '{none}' THEN files - named ELSE 0 END),
                   -- 서버가 이름 없음으로 확정한 것
                   SUM(CASE WHEN status = '{none}' THEN files - named ELSE 0 END),
                   COUNT(CASE WHEN (status IS NULL OR status <> '{none}') AND files > named THEN 1 END),
                   -- 오프라인이 아직 손대지 않은 자리 — targets(Offline) 과 같은 조건
                   COUNT(CASE WHEN files > named
                               AND (status IS NULL
                                    OR (status = '{unresolved}'
                                        AND COALESCE(source,'') <> '{offline}')) THEN 1 END),
                   -- 서버에만 물을 수 있는 자리: 오프라인이 이미 포기한 곳
                   COUNT(CASE WHEN status = '{unresolved}' AND files > named THEN 1 END),
                   -- 서버에 물을 수 있는 자리 — targets(Online) 과 같은 조건이어야
                   -- 화면의 수와 실제로 도는 수가 어긋나지 않는다
                   COUNT(CASE WHEN (files > named OR source = '{offline}')
                               AND (online_outcome IS NULL
                                    OR (?1 IS NOT NULL
                                        AND (online_provider IS NULL
                                             OR online_provider <> ?1))) THEN 1 END),
                   -- 가진 값을 옮겨 붙이기만 하면 되는 자리. 파일 표를 한 번 더
                   -- 훑지 않으려고 같은 질의 안에서 센다 (실측 115→284ms 였다)
                   COUNT(CASE WHEN files > named AND status = '{ok}'
                               AND country IS NOT NULL AND trim(country) <> '' THEN 1 END)
                 FROM joined",
                cell = cell_sql("gps_lat", "gps_lon"),
                none = NONE, unresolved = UNRESOLVED, offline = SRC_OFFLINE, online = SRC_ONLINE,
                ok = OK
            ),
            rusqlite::params![provider],
            |r| {
                Ok(Stats {
                    with_gps: r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    named: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    approximate_files: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    precise_files: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    pending_files: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    unavailable_files: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    cells_left: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    offline_cells_left: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    network_cells_left: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    online_cells_left: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    cache_cells_left: r.get::<_, Option<i64>>(10)?.unwrap_or(0),
                    endpoint_ready: false,
                })
            },
        )
    })?;
    stats.endpoint_ready = endpoint_ready;
    Ok(stats)
}
