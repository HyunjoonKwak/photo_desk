use super::*;

/// 사이드바의 갈래 하나 — 값·표시 이름·장수.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Facet {
    pub value: String,
    pub label: String,
    pub count: i64,
}
/// 사이드바가 훑어볼 갈래.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetKind {
    Year,
    /// 하루 단위. 달력이 한 달을 펼쳤을 때 쓴다 — 필터에 month를 함께 건다.
    Day,
    Camera,
    Lens,
    Rating,
    Kind,
    Place,
    /// 지명 — 국가 / 시도 / 시군구. 위쪽 단계를 필터에 걸고 다음 단계를 센다
    Country,
    Admin1,
    Admin2,
}

/// 지금 필터 안에서 각 값이 몇 장인지 센다.
///
/// 필터를 함께 거는 이유: 「2020년」을 고른 뒤 카메라 목록을 보면 그해에 쓴
/// 카메라만 나와야 한다. 전체 목록이 나오면 눌러도 0장인 것이 섞인다.
pub fn facets(db: &Db, f: &Filter, kind: FacetKind) -> Result<Vec<Facet>> {
    let gps = valid_gps_sql();
    let (where_sql, params) = build_where(f, None);
    let join = if needs_folder_join(f) {
        "JOIN folders fo ON fo.id = fi.folder_id"
    } else {
        ""
    };
    let expr: String = match kind {
        FacetKind::Year => "strftime('%Y', fi.taken_at,'unixepoch','localtime')".into(),
        FacetKind::Day => "strftime('%Y-%m-%d', fi.taken_at,'unixepoch','localtime')".into(),
        FacetKind::Camera => "COALESCE(NULLIF(fi.cam_model,''),'')".into(),
        FacetKind::Lens => "COALESCE(NULLIF(fi.lens,''),'')".into(),
        FacetKind::Rating => "CAST(fi.rating AS TEXT)".into(),
        FacetKind::Kind => "CAST(fi.kind AS TEXT)".into(),
        // 좌표를 0.1도 격자로 내린다. 역지오코딩이 없어 지명은 못 붙이지만
        // "이 근처에서 찍은 것"을 모아 보는 데는 충분하다. ROUND(x-.5)는
        // 정확한 0을 -0.1로 보내므로 쓰지 않는다. 좌표를 10배해 소수 오차를
        // 먼저 자르고 양수로 옮긴 뒤 CAST하면 음수·경계값도 같은 칸으로 간다.
        FacetKind::Country => "COALESCE(fi.geo_country,'')".into(),
        FacetKind::Admin1 => "COALESCE(fi.geo_admin1,'')".into(),
        FacetKind::Admin2 => "COALESCE(fi.geo_admin2,'')".into(),
        FacetKind::Place => format!(
            "CASE WHEN NOT ({gps}) THEN ''
                  ELSE printf('%.1f,%.1f',
                    CAST(ROUND(fi.gps_lat * 10.0, 8) + 900.0 AS INTEGER) / 10.0 - 90.0,
                    CAST(ROUND(fi.gps_lon * 10.0, 8) + 1800.0 AS INTEGER) / 10.0 - 180.0)
             END"
        ),
    };
    let order = match kind {
        // 연도·평점은 값 순서로, 카메라는 많이 쓴 것부터
        FacetKind::Year | FacetKind::Day | FacetKind::Rating => "v DESC",
        FacetKind::Place => "n DESC, v",
        _ => "n DESC, v",
    };
    let sql = format!(
        "SELECT {expr} v, COUNT(*) n FROM files fi {join} {where_sql}
         GROUP BY v ORDER BY {order} LIMIT 200"
    );
    let rows: Vec<(String, i64)> = db.read(|c| {
        let mut st = c.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let it = st.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;

    Ok(rows
        .into_iter()
        .map(|(value, count)| {
            let label = match kind {
                FacetKind::Year => format!("{value}년"),
                // `2024-08-27` → `27일`. 어느 달인지는 위에 펼쳐진 줄이 말한다.
                FacetKind::Day => value
                    .rsplit('-')
                    .next()
                    .and_then(|d| d.parse::<u32>().ok())
                    .map(|d| format!("{d}일"))
                    .unwrap_or_else(|| value.clone()),
                FacetKind::Rating => match value.as_str() {
                    "0" => "평점 없음".into(),
                    n => "★".repeat(n.parse::<usize>().unwrap_or(0)),
                },
                FacetKind::Kind => match value.as_str() {
                    "0" => "사진".into(),
                    "1" => "영상".into(),
                    _ => "RAW".into(),
                },
                FacetKind::Country | FacetKind::Admin1 | FacetKind::Admin2 => {
                    if value.is_empty() {
                        "(지명 없음)".into()
                    } else {
                        value.clone()
                    }
                }
                FacetKind::Camera => {
                    if value.is_empty() {
                        "(카메라 정보 없음)".into()
                    } else {
                        value.clone()
                    }
                }
                FacetKind::Lens => {
                    if value.is_empty() {
                        "(렌즈 정보 없음)".into()
                    } else {
                        value.clone()
                    }
                }
                FacetKind::Place => {
                    if value.is_empty() {
                        "(위치 정보 없음)".into()
                    } else {
                        // `37.5,127` → `북위 37.5° 동경 127.0°`
                        match value.split_once(',') {
                            Some((a, b)) => {
                                let lat: f64 = a.parse().unwrap_or(0.0);
                                let lon: f64 = b.parse().unwrap_or(0.0);
                                format!(
                                    "{} {:.1}° {} {:.1}°",
                                    if lat >= 0.0 { "북위" } else { "남위" },
                                    lat.abs(),
                                    if lon >= 0.0 { "동경" } else { "서경" },
                                    lon.abs(),
                                )
                            }
                            None => value.clone(),
                        }
                    }
                }
            };
            Facet {
                value,
                label,
                count,
            }
        })
        .collect())
}
