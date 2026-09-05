//! 지명 — 좌표를 «국가 / 시도 / 시군구» 세 단계 이름으로.
//!
//! 사진마다 묻지 않는다. 좌표를 **0.01도 격자**(약 1.1km)로 뭉쳐 격자마다 한 번만
//! 물어보고 `places` 에 캐시한다 — 실측(2026-09-01) 사진 52,576장이 격자로는
//! 1,143칸뿐이다. 한 번 채우면 그 뒤로는 완전히 오프라인이다.
//!
//! 이름은 Nominatim 규약을 쓰는 서버에서 받는다. 배포 앱 여러 대의 요청을 합쳐
//! 초당 한 건이어야 하는 OSM 공개 서버는 배치 작업에 쓰지 않는다. 설정
//! (`geo.endpoint`)에 자체 Nominatim이나 배치 사용이 허용된 서비스를 넣었을 때만
//! 새 좌표를 묻는다. 이미 받은 캐시는 서버가 없어도 쓴다.
//!
//! 서버에는 초당 한 건만 보내고, 앱을 밝히는 User-Agent를 쓰며, HTTP 오류가 오면
//! 그 자리에서 멈춘다. 물어보는 좌표는 그 칸에 실제로 있는 사진들의 대표 좌표이고,
//! 한 번 물어본 것은 places 에 남아 두 번 묻지 않는다.

pub mod boundary;
pub mod offline;
pub mod resolve;

mod fill;
mod online;

pub use fill::{fill, fill_offline, propagate_library, stats, Mode, Progress, Stats};

use crate::db::conn::{Db, Result};
use std::time::Duration;

/// 격자 한 칸 — 0.01도. 이보다 잘게 나누면 같은 동네를 여러 번 묻는다.
const CELL: f64 = 0.01;
/// Nominatim 규칙 — 초당 한 건.
const GAP: Duration = Duration::from_millis(1100);
const UA: &str = concat!(
    "photo-desk/",
    env!("CARGO_PKG_VERSION"),
    " (personal photo library; github.com/HyunjoonKwak/photo_desk)"
);
const ENDPOINT_KEY: &str = "geo.endpoint";
const PUBLIC_HOST: &str = "nominatim.openstreetmap.org";
/// 캐시 한 줄의 상태 — 이 셋을 섞으면 «결과 없음»을 영영 다시 묻는다 (2026-09-01 리뷰)
const OK: &str = "ok";
/// 그 자리에 이름이 없다고 **온라인 서버가 확정**했다. 다시 묻지 않는다
const NONE: &str = "none";
/// 오프라인으로 안전하게 정하지 못했다 — 온라인으로 다시 물을 수 있다.
/// none 과 섞으면 «물어볼 수 있는 것»을 영영 잃는다 (2026-09-01 리뷰)
const UNRESOLVED: &str = "unresolved";
/// 출처 — 어디서 온 값인가
const SRC_OFFLINE: &str = "offline_geonames";
const SRC_ONLINE: &str = "nominatim";
/// 정밀도 — 얼마나 믿을 만한가
const PREC_APPROX: &str = "approximate";
const PREC_BOUNDARY: &str = "boundary";
const PREC_REMOTE: &str = "remote";

/// 지도와 같은 «쓸 수 있는 좌표» 규칙 — 판정은 db::predicates 가 갖는다.
/// 통계·대상 선택·파일 갱신이 반드시 같은 조건을 써야, 처리할 수 없는 행을
/// 영원히 남은 것으로 세지 않는다. 이 모듈의 질의는 별칭 없이 files 를 읽는다.
fn valid_gps_sql() -> String {
    crate::db::predicates::valid_gps_sql(crate::db::predicates::Files::Bare)
}

fn validate_endpoint(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(crate::db::conn::DbError::Invalid(
            "설정 › 탐색에서 자체 Nominatim 또는 배치 사용이 허용된 지명 서버를 먼저 입력해 주세요"
                .into(),
        ));
    }
    let url = reqwest::Url::parse(raw).map_err(|_| {
        crate::db::conn::DbError::Invalid("지명 서버 주소가 올바른 URL이 아닙니다".into())
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(crate::db::conn::DbError::Invalid(
            "지명 서버는 http 또는 https 주소여야 합니다".into(),
        ));
    }
    if url
        .host_str()
        .is_some_and(|h| h.trim_end_matches('.').eq_ignore_ascii_case(PUBLIC_HOST))
    {
        return Err(crate::db::conn::DbError::Invalid(
            "OSM 공개 Nominatim은 배포 앱의 대량 조회에 사용할 수 없습니다 — 자체 서버나 배치 사용이 허용된 서비스를 입력해 주세요".into(),
        ));
    }
    Ok(raw.to_string())
}

fn endpoint_setting(db: &Db) -> Result<Option<String>> {
    Ok(crate::db::settings::get(db, ENDPOINT_KEY)?.filter(|s| !s.trim().is_empty()))
}

/// SQLite 로 좌표를 격자 문자열로 — `FLOOR` 는 수학 확장이 있어야 해서 쓰지 않는다.
/// CAST 는 0 쪽으로 자르므로 음수면 1을 뺀다 (내림).
fn cell_sql(lat: &str, lon: &str) -> String {
    let floor = |c: &str| {
        format!(
            "(CAST({c} * 100.0 AS INTEGER) - ({c} * 100.0 < CAST({c} * 100.0 AS INTEGER))) / 100.0"
        )
    };
    format!("printf('%.2f,%.2f', {}, {})", floor(lat), floor(lon))
}

/// 좌표 → 격자 열쇠. 음수도 같은 칸에 들어가게 내림으로 자른다.
pub fn cell(lat: f64, lon: f64) -> String {
    format!(
        "{:.2},{:.2}",
        (lat / CELL).floor() * CELL,
        (lon / CELL).floor() * CELL
    )
}

/// 격자 열쇠 → 그 칸의 한가운데 좌표. 이 점을 물어본다.
pub fn cell_center(cell: &str) -> Option<(f64, f64)> {
    let (a, b) = cell.split_once(',')?;
    Some((
        a.parse::<f64>().ok()? + CELL / 2.0,
        b.parse::<f64>().ok()? + CELL / 2.0,
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Place {
    pub country: Option<String>,
    /// 시도 — 경기도 · 서울특별시 · 뉴사우스웨일스주
    pub admin1: Option<String>,
    /// 시군구 — 수원시 · 서초구 · 시드니
    pub admin2: Option<String>,
}

impl Place {
    /// 몇 단계까지 채워졌나 — 0(빈 값)부터 3(시군구까지).
    ///
    /// 위가 비면 아래도 세지 않는다. «나라 없이 시군구만» 같은 결과는 트리에
    /// 걸 자리가 없어 없는 것과 같다.
    pub fn depth(&self) -> u8 {
        match (&self.country, &self.admin1, &self.admin2) {
            (Some(_), Some(_), Some(_)) => 3,
            (Some(_), Some(_), None) => 2,
            (Some(_), _, _) => 1,
            _ => 0,
        }
    }
    /// 표시용 — 가장 좁은 단계. 셋 다 비면 None
    pub fn name(&self) -> Option<String> {
        self.admin2
            .clone()
            .or_else(|| self.admin1.clone())
            .or_else(|| self.country.clone())
    }
    pub fn is_empty(&self) -> bool {
        self.country.is_none() && self.admin1.is_none() && self.admin2.is_none()
    }
}

/// Nominatim 의 주소 조각을 세 단계로 접는다.
///
/// 나라마다 어느 칸이 오는지가 달라 «후보 목록 + 승격» 규칙을 쓴다:
/// 시도 후보가 비어 있고 시군구 후보가 둘 이상이면 첫째를 시도로 올린다.
/// (서울은 state 가 없고 city=서울특별시 · borough=서초구 로 온다)
pub fn fold(addr: &serde_json::Value) -> Place {
    let get = |k: &str| {
        addr.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let first = |keys: &[&str]| keys.iter().find_map(|k| get(k));

    let country = get("country");
    let lvl1 = first(&["state", "province", "region", "state_district"]);
    let lvl2: Vec<String> = [
        "city",
        "county",
        "municipality",
        "town",
        "borough",
        "city_district",
        "village",
        "suburb",
    ]
    .iter()
    .filter_map(|k| get(k))
    .collect();
    // 같은 이름이 두 칸에 겹쳐 오는 경우가 있다 (city=수원시, county=수원시)
    let mut uniq: Vec<String> = Vec::new();
    for v in lvl2 {
        if !uniq.contains(&v) && Some(&v) != lvl1.as_ref() {
            uniq.push(v);
        }
    }
    match lvl1 {
        Some(a1) => Place {
            country,
            admin1: Some(a1),
            admin2: uniq.into_iter().next(),
        },
        None if uniq.len() >= 2 => {
            let mut it = uniq.into_iter();
            Place {
                country,
                admin1: it.next(),
                admin2: it.next(),
            }
        }
        None => Place {
            country,
            admin1: uniq.into_iter().next(),
            admin2: None,
        },
    }
}

#[cfg(test)]
mod tests;

/// 실측용 — 사용자 DB를 복사해 오프라인 채우기를 재 본다.
///
/// `ACUT_BENCH_DB` 에 DB 경로를 주고 `cargo test --lib -- --ignored bench` 로 돌린다.
/// 사용자 DB를 열지 않고 임시 사본만 건드린다.
#[cfg(test)]
mod bench;
