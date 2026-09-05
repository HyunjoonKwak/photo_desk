use super::*;

/// 필터에 걸리는 전체 개수와 용량. 페이지마다 세지 않고 필터가 바뀔 때만 호출한다.
/// 지도의 칸 하나 — 이 칸에 든 사진 수와 대표 썸네일
#[derive(Debug, Clone, serde::Serialize)]
pub struct MapCell {
    pub lat: f64,
    pub lon: f64,
    pub n: i64,
    pub library_id: Option<i64>,
    pub thumb: Option<String>,
    /// 이 칸에서 가장 흔한 지명 — 지도에서 위경도만 보고 어디인지 알 수 없던 것을 푼다
    pub place: Option<String>,
    /// 이 칸에 섞인 서로 다른 지명 수. 2 이상이면 «외 N곳»으로 알린다.
    pub places: i64,
}

/// 지도 전체 조건의 요약. 마커는 현재 화면만 읽되, 장수와 첫 자동 맞춤은
/// 잘린 마커 목록이 아니라 이 전역 요약을 사용한다.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MapOverview {
    pub total: i64,
    /// [남, 서, 북, 동]. 쓸 수 있는 좌표가 없으면 None.
    pub bounds: Option<[f64; 4]>,
}

pub fn map_overview(db: &Db, f: &Filter) -> Result<MapOverview> {
    let gps = valid_gps_sql();
    let (where_sql, params) = build_where(f, None);
    let join = if needs_folder_join(f) {
        "JOIN folders fo ON fo.id = fi.folder_id"
    } else {
        ""
    };
    let sql = format!(
        "SELECT COUNT(*), MIN(fi.gps_lat), MIN(fi.gps_lon), MAX(fi.gps_lat), MAX(fi.gps_lon)
           FROM files fi {join} {where_sql} AND ({gps})"
    );
    db.read(|c| {
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        c.query_row(&sql, refs.as_slice(), |r| {
            let total: i64 = r.get(0)?;
            let bounds = if total == 0 {
                None
            } else {
                Some([r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])
            };
            Ok(MapOverview { total, bounds })
        })
    })
}

/// 조건에 맞는 사진을 `precision`도 격자로 묶는다 — 지도가 확대될수록 잘게.
/// 칸마다 평균 좌표, 장수, 대표 한 장. 부르는 쪽이 현재 지도 화면을 bbox로
/// 넣으므로 4,000개 제한을 넘어도 다른 지역으로 이동하면 그곳을 다시 읽는다.
pub fn map_cells(db: &Db, f: &Filter, precision: f64) -> Result<Vec<MapCell>> {
    let gps = valid_gps_sql();
    // clamp 는 NaN 을 그대로 통과시킨다 — 그 값이 SQL 글월에 박히면 «그런 칸이
    // 없다»는 오류로 지도가 통째로 빈다. 숫자가 아니면 기본 칸으로 돌린다.
    let p = if precision.is_finite() {
        precision.clamp(0.0001, 10.0)
    } else {
        0.1
    };
    let (where_sql, params) = build_where(f, None);
    let join = if needs_folder_join(f) {
        "JOIN folders fo ON fo.id = fi.folder_id"
    } else {
        ""
    };
    // +90/+180으로 양수로 만든 뒤 자른다 — CAST는 0쪽으로 자르므로 음수면 칸이 어긋난다
    //
    // 칸마다 «가장 흔한 지명»을 함께 뽑는다. 사전순 첫째(MIN)를 쓰면 그 칸을
    // 대표하지 못하는 이름이 뽑힌다 — 한 장짜리 옆 동네가 이길 수 있다.
    // 줌을 당기면 한 칸에 여러 곳이 섞이므로 그 수(places)도 함께 센다.
    let sql = format!(
        "WITH pts AS MATERIALIZED (
           SELECT CAST((fi.gps_lat + 90.0) / {p} AS INTEGER) AS ky,
                  CAST((fi.gps_lon + 180.0) / {p} AS INTEGER) AS kx,
                  fi.gps_lat AS la, fi.gps_lon AS lo, fi.id AS id, fi.geo_name AS place
             FROM files fi {join} {where_sql} AND ({gps})
         ),
         agg AS (
           SELECT ky, kx, AVG(la) AS la, AVG(lo) AS lo, COUNT(*) AS n, MAX(id) AS id,
                  COUNT(DISTINCT place) AS places
             FROM pts GROUP BY ky, kx ORDER BY n DESC LIMIT 4000
         ),
         top AS (
           SELECT ky, kx, place,
                  ROW_NUMBER() OVER (PARTITION BY ky, kx ORDER BY COUNT(*) DESC, place) AS rn
             FROM pts WHERE place IS NOT NULL GROUP BY ky, kx, place
         )
         SELECT a.la, a.lo, a.n, a.id, t.place, a.places
           FROM agg a LEFT JOIN top t ON t.ky = a.ky AND t.kx = a.kx AND t.rn = 1
          ORDER BY a.n DESC"
    );
    let cells: Vec<(f64, f64, i64, i64, Option<String>, i64)> = db.read(|c| {
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let mut st = c.prepare(&sql)?;
        let it = st.query_map(refs.as_slice(), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    // 대표 썸네일 — 칸마다 한 장, 한 번에 묻는다
    let ids: Vec<i64> = cells.iter().map(|c| c.3).collect();
    let covers: std::collections::HashMap<i64, (i64, String)> = if ids.is_empty() {
        Default::default()
    } else {
        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT fi.id, fo.library_id, t.rel_path FROM files fi
               JOIN folders fo ON fo.id = fi.folder_id
               JOIN thumbs t ON t.file_id = fi.id AND t.state = 1
              WHERE fi.id IN ({marks})"
        );
        db.read(|c| {
            let refs: Vec<&dyn rusqlite::ToSql> =
                ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            let mut st = c.prepare(&sql)?;
            let it = st.query_map(refs.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?)))
            })?;
            it.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()
        })?
    };
    Ok(cells
        .into_iter()
        .map(|(lat, lon, n, id, place, places)| {
            let (library_id, thumb) = match covers.get(&id) {
                Some((l, t)) => (Some(*l), Some(t.clone())),
                None => (None, None),
            };
            MapCell {
                lat,
                lon,
                n,
                library_id,
                thumb,
                place,
                places,
            }
        })
        .collect())
}
