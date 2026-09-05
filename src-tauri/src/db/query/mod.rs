//! 목록 조회 — 그리드가 쓰는 쿼리.
//!
//! **keyset 페이지네이션을 쓴다.** `OFFSET`은 앞의 행을 전부 세면서 지나가므로
//! 뒤 페이지일수록 느려진다. 6만 장에서 스크롤을 끝까지 내리면 체감된다.
//! `WHERE taken_at < ?`는 인덱스에서 그 지점을 바로 찾으므로 어디서나 같은 속도다.
//!
//! 커서는 `(taken_at, id)` 쌍이다. 같은 시각의 사진이 여럿일 수 있어 id로 동점을
//! 가른다. 이게 없으면 경계에서 사진이 빠지거나 겹친다.

use crate::db::conn::{Db, Result};
use rusqlite::OptionalExtension;

mod facets;
mod map;

pub use facets::{facets, Facet, FacetKind};
pub use map::{map_cells, map_overview, MapCell, MapOverview};

/// 그리드 한 칸에 필요한 것만. 인스펙터용 상세는 따로 가져온다.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileRow {
    pub id: i64,
    pub name: String,
    pub taken_at: i64,
    pub taken_at_source: i32,
    pub kind: i32,
    pub size: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub rating: i32,
    pub culling_flag: i32,
    pub favorite: bool,
    /// 영상 길이. 타일의 ▶ 배지에 쓴다.
    pub duration_ms: Option<i64>,
    /// 정렬 커서를 만들 때 쓴다 (생성일·수정일 기준 정렬)
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
    /// 그룹 머리글에 쓸 값. 묶기를 끄면 비어 있다.
    pub group: Option<String>,
    /// 어느 라이브러리 소속인가. 썸네일 캐시가 라이브러리마다 따로 있어서
    /// 프론트가 `thumb://` 주소를 만들 때 필요하다.
    pub library_id: Option<i64>,
    /// 캐시 루트 기준 상대경로. 없으면 아직 생성 전이다.
    pub thumb: Option<String>,
    /// 타일 배지용 — 설정에서 ISO·셔터·조리개·초점거리 중 하나를 고른다
    pub iso: Option<i64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub focal_mm: Option<f64>,
    pub cam_model: Option<String>,
}

/// 무엇으로 정렬할까. Lap의 정렬 목록과 같다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    #[default]
    TakenAt,
    CreatedAt,
    ModifiedAt,
    Name,
    Size,
    Pixels,
    Duration,
}

impl SortBy {
    /// 정렬에 쓸 식. NULL은 맨 뒤로 몰리게 COALESCE로 채운다 — 안 그러면
    /// 커서 비교에서 NULL이 끼어 페이지가 끊긴다.
    fn expr(self) -> &'static str {
        match self {
            SortBy::TakenAt => "fi.taken_at",
            SortBy::CreatedAt => "COALESCE(fi.created_at, 0)",
            SortBy::ModifiedAt => "COALESCE(fi.modified_at, 0)",
            SortBy::Name => "fi.name",
            SortBy::Size => "fi.size",
            SortBy::Pixels => "COALESCE(fi.width,0) * COALESCE(fi.height,0)",
            SortBy::Duration => "COALESCE(fi.duration_ms, 0)",
        }
    }
    fn is_text(self) -> bool {
        matches!(self, SortBy::Name)
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Sort {
    pub by: SortBy,
    /// 큰 것부터. 촬영일은 최신순이 기본이다.
    pub desc: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            by: SortBy::TakenAt,
            desc: true,
        }
    }
}

/// 다음 페이지를 가리키는 커서.
///
/// 정렬 기준 값과 id를 함께 들고 다닌다. id가 없으면 같은 값이 여럿일 때
/// 경계에서 사진이 빠지거나 겹친다.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Cursor {
    /// 숫자 기준일 때의 값
    pub num: Option<i64>,
    /// 이름 기준일 때의 값
    pub text: Option<String>,
    pub id: i64,
}

/// `#[serde(default)]`가 중요하다. 프론트는 필요한 필드만 보낸다 —
/// 없는 필드에서 역직렬화가 실패하면 커맨드 전체가 거부된다.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Filter {
    /// 등록한 라이브러리 하나만. None이면 전체.
    pub library_id: Option<i64>,
    /// 이 폴더와 하위 폴더. None이면 전체.
    pub folder_id: Option<i64>,
    /// 볼륨 기준 폴더 경로. 이 폴더와 하위 폴더를 고른다.
    ///
    /// 사이드바 트리에는 DB에 행이 없는 중간 마디가 있어서 id로는 못 고른다.
    /// (`연도별`처럼 자기 자신엔 사진이 없고 아래에만 있는 폴더)
    pub folder_path: Option<String>,
    /// 0 작업대 · 1 내사진 · 2 공용
    pub area: Option<i32>,
    /// 0 사진 · 1 영상 · 2 RAW
    pub kind: Option<i32>,
    /// 이 값 이상만
    pub min_rating: Option<i32>,
    /// 0 미판정 · 1 남김 · 2 제외
    pub culling_flag: Option<i32>,
    pub favorite_only: bool,
    /// 파일명 부분 일치
    pub name_like: Option<String>,
    /// true면 **휴지통에 든 것만** 본다. 기본은 살아 있는 것만.
    pub trashed: bool,
    /// 무엇으로 어떤 방향으로 늘어놓을까.
    pub sort: Sort,
    /// 사이드바에서 고른 연도 (`2024`)
    pub year: Option<String>,
    /// 사이드바에서 고른 달 (`2024-08`)
    pub month: Option<String>,
    /// 사이드바에서 고른 날 (`2024-08-27`)
    pub day: Option<String>,
    /// 지명 3단계 — 국가 / 시도 / 시군구. 빈 문자열이면 «이름 없음»
    pub country: Option<String>,
    pub admin1: Option<String>,
    pub admin2: Option<String>,
    /// 사이드바에서 고른 카메라 모델
    pub camera: Option<String>,
    /// 사이드바에서 고른 렌즈. 빈 문자열이면 "렌즈 정보 없음".
    pub lens: Option<String>,
    /// 사이드바에서 고른 태그
    pub tag_id: Option<i64>,
    /// 위치 — 좌표 격자 한 칸 (`37.5,127.0`). 빈 문자열이면 "위치 없음".
    pub place: Option<String>,
    /// 썸네일이 없는 것만 — 못 만들었거나 아직 안 만든 것. 상태바 «썸네일 없음
    /// N장»을 누르면 걸린다. 무엇이 안 되는지 눈으로 봐야 한다.
    #[serde(default)]
    pub no_thumb: bool,
    /// 이 사람이 나온 사진만 (faces.person_id)
    #[serde(default)]
    pub person_id: Option<i64>,
    /// 지도에서 고른 영역 — `남,서,북,동` (도). 지도 갈래에서 칸이나 보이는 영역을 누르면 걸린다.
    #[serde(default)]
    pub bbox: Option<String>,
    /// NAS에 있는지 확인된 것만(true) / 확인 안 된 것만(false)
    #[serde(default)]
    pub nas: Option<bool>,
}

/// 그리드에 머리글을 넣어 묶는 기준. Lap의 GROUP과 같다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    #[default]
    None,
    Folder,
    Day,
    Month,
    Year,
    Rating,
    Camera,
    Lens,
    FileType,
    Culling,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub rows: Vec<FileRow>,
    pub next: Option<Cursor>,
}

/// 지도와 위치 갈래가 함께 쓰는 «쓸 수 있는 좌표» 규칙.
///
/// 일부 카메라·내보내기 도구는 위치가 없을 때 NULL 대신 (0, 0)을 쓴다.
/// 실제 라이브러리에도 그런 행이 수천 장 있어 그대로 지도에 넣으면 Null Island가
/// 가장 큰 장소가 되고 자동 맞춤도 세계 전체로 벌어진다. 두 값이 모두 정확히 0인
/// 경우만 센티널로 보고, 한쪽 누락·지구 범위 밖 좌표도 위치 없음으로 친다.
/// 파일 표의 별칭은 이 모듈 전체에서 `fi` 다. 판정 자체는 db::predicates 가 갖는다 —
/// geo.rs 와 같은 뜻이어야 «세는 사진»과 «처리하는 사진»이 어긋나지 않는다
pub(super) fn valid_gps_sql() -> String {
    crate::db::predicates::valid_gps_sql(crate::db::predicates::Files::Fi)
}

/// LIKE 와일드카드를 이스케이프한다. `_`가 임의 문자로 동작하면
/// `IMG_1234` 검색이 엉뚱한 것까지 잡는다.
/// `남,서,북,동` → [남, 서, 북, 동]. 지구 범위 밖·무한대·뒤집힌 상자는 거절한다.
pub fn parse_bbox(s: &str) -> Option<[f64; 4]> {
    let v: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse())
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if v.len() != 4
        || v.iter().any(|x| !x.is_finite())
        || !(-90.0..=90.0).contains(&v[0])
        || !(-90.0..=90.0).contains(&v[2])
        || !(-180.0..=180.0).contains(&v[1])
        || !(-180.0..=180.0).contains(&v[3])
        || v[0] > v[2]
        || v[1] > v[3]
    {
        return None;
    }
    Some([v[0], v[1], v[2], v[3]])
}

pub(crate) fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// WHERE 절이 `folders`를 실제로 보는가.
///
/// 안 볼 때는 조인을 빼야 한다. `files`만 훑으면 되는 집계에서 14만 번의
/// rowid 조회가 통째로 사라진다 (실측 타임라인 395ms -> 240ms).
pub(super) fn needs_folder_join(f: &Filter) -> bool {
    f.area.is_some() || f.library_id.is_some() || f.folder_path.is_some()
}

/// 필터를 WHERE 절과 파라미터로 바꾼다.
pub(super) fn build_where(
    f: &Filter,
    cursor: Option<Cursor>,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let gps = valid_gps_sql();
    let mut w: Vec<String> = Vec::new();
    let mut p: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    // 버린 것은 기본적으로 안 보인다. 이게 첫 조건이어야 한다 — 빼먹으면
    // 휴지통에 넣은 사진이 목록에 계속 남아 있고 원본은 없어 썸네일만 뜬다.
    w.push(
        if f.trashed {
            "fi.trashed_at IS NOT NULL"
        } else {
            "fi.trashed_at IS NULL"
        }
        .into(),
    );

    if let Some(id) = f.library_id {
        w.push("fo.library_id = ?".into());
        p.push(Box::new(id));
    }
    if let Some(p_) = f.folder_path.as_deref().filter(|s| !s.is_empty()) {
        // LIKE는 `_`와 `%`를 와일드카드로 본다. 실제 폴더에 `#0_사진백업…`
        // 같은 이름이 있어 이스케이프가 필수다.
        w.push("(fo.rel_path = ? OR fo.rel_path LIKE ? ESCAPE '\\')".into());
        p.push(Box::new(p_.to_string()));
        p.push(Box::new(format!("{}/%", escape_like(p_))));
    }
    if let Some(id) = f.folder_id {
        // 하위 폴더까지 포함한다. rel_path 접두사로 찾는다 — LIKE 는 폴더명의 `_`·`%`가
        // 와일드카드가 되니 substr 로 정확히 비교한다 (폴더 행 2만 개, 값싸다)
        w.push(
            "fi.folder_id IN (SELECT id FROM folders WHERE id = ?
              OR (volume_uuid = (SELECT volume_uuid FROM folders WHERE id = ?)
                  AND substr(rel_path, 1, length((SELECT rel_path FROM folders WHERE id = ?)) + 1)
                      = (SELECT rel_path FROM folders WHERE id = ?) || '/'))"
                .into(),
        );
        p.push(Box::new(id));
        p.push(Box::new(id));
        p.push(Box::new(id));
        p.push(Box::new(id));
    }
    if let Some(a) = f.area {
        w.push("fo.area = ?".into());
        p.push(Box::new(a));
    }
    if let Some(k) = f.kind {
        w.push("fi.kind = ?".into());
        p.push(Box::new(k));
    }
    if let Some(r) = f.min_rating {
        w.push("fi.rating >= ?".into());
        p.push(Box::new(r));
    }
    if let Some(c) = f.culling_flag {
        w.push("fi.culling_flag = ?".into());
        p.push(Box::new(c));
    }
    if let Some(y) = f.year.as_deref().filter(|s| !s.is_empty()) {
        w.push("strftime('%Y', fi.taken_at,'unixepoch','localtime') = ?".into());
        p.push(Box::new(y.to_string()));
    }
    if let Some(m) = f.month.as_deref().filter(|s| !s.is_empty()) {
        w.push("strftime('%Y-%m', fi.taken_at,'unixepoch','localtime') = ?".into());
        p.push(Box::new(m.to_string()));
    }
    if let Some(d) = f.day.as_deref().filter(|s| !s.is_empty()) {
        w.push("strftime('%Y-%m-%d', fi.taken_at,'unixepoch','localtime') = ?".into());
        p.push(Box::new(d.to_string()));
    }
    for (col, val) in [
        ("fi.geo_country", f.country.as_ref()),
        ("fi.geo_admin1", f.admin1.as_ref()),
        ("fi.geo_admin2", f.admin2.as_ref()),
    ] {
        if let Some(v) = val {
            if v.is_empty() {
                w.push(format!("{col} IS NULL"));
            } else {
                w.push(format!("{col} = ?"));
                p.push(Box::new(v.clone()));
            }
        }
    }
    if let Some(cam) = f.camera.as_ref() {
        // 빈 문자열은 "카메라 정보 없음"을 뜻한다
        if cam.is_empty() {
            w.push("COALESCE(NULLIF(fi.cam_model,''),'') = ''".into());
        } else {
            w.push("fi.cam_model = ?".into());
            p.push(Box::new(cam.clone()));
        }
    }
    if let Some(l) = f.lens.as_ref() {
        if l.is_empty() {
            w.push("COALESCE(NULLIF(fi.lens,''),'') = ''".into());
        } else {
            w.push("fi.lens = ?".into());
            p.push(Box::new(l.clone()));
        }
    }
    if let Some(t) = f.tag_id {
        w.push(
            "EXISTS (SELECT 1 FROM file_tags ft WHERE ft.file_id = fi.id AND ft.tag_id = ?)".into(),
        );
        p.push(Box::new(t));
    }
    if let Some(pl) = f.place.as_ref() {
        if pl.is_empty() {
            w.push(format!("NOT ({gps})"));
        } else if let Some((a, b)) = pl.split_once(',') {
            // 격자 한 칸 = 0.1도 (위도로 약 11km). 그 칸 안이면 같은 곳으로 친다.
            if let (Ok(lat), Ok(lon)) = (a.parse::<f64>(), b.parse::<f64>()) {
                w.push(format!(
                    "({gps}) AND fi.gps_lat >= ? AND fi.gps_lat < ?
                     AND fi.gps_lon >= ? AND fi.gps_lon < ?"
                ));
                p.push(Box::new(lat));
                p.push(Box::new(lat + 0.1));
                p.push(Box::new(lon));
                p.push(Box::new(lon + 0.1));
            }
        }
    }
    if f.favorite_only {
        w.push("fi.favorite = 1".into());
    }
    if f.no_thumb {
        w.push(
            "NOT EXISTS (SELECT 1 FROM thumbs t WHERE t.file_id = fi.id AND t.state = 1)".into(),
        );
    }
    if let Some(b) = f.bbox.as_deref().and_then(parse_bbox) {
        w.push(format!(
            "({gps}) AND fi.gps_lat >= ? AND fi.gps_lat <= ?
             AND fi.gps_lon >= ? AND fi.gps_lon <= ?"
        ));
        p.push(Box::new(b[0]));
        p.push(Box::new(b[2]));
        p.push(Box::new(b[1]));
        p.push(Box::new(b[3]));
    }
    if let Some(on) = f.nas {
        w.push(if on {
            "EXISTS (SELECT 1 FROM nas_state n WHERE n.file_id = fi.id)".into()
        } else {
            "NOT EXISTS (SELECT 1 FROM nas_state n WHERE n.file_id = fi.id)".into()
        });
    }
    if let Some(pid) = f.person_id {
        w.push(
            "EXISTS (SELECT 1 FROM faces fa WHERE fa.file_id = fi.id AND fa.person_id = ?)".into(),
        );
        p.push(Box::new(pid));
    }
    if let Some(q) = f
        .name_like
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        w.push("fi.name LIKE ? ESCAPE '\\'".into());
        p.push(Box::new(format!("%{}%", escape_like(q))));
    }
    // 커서 — 정렬과 **같은 방향**이어야 한다. 방향이 어긋나면 페이지가
    // 겹치거나 통째로 건너뛴다.
    if let Some(c) = cursor {
        let col = f.sort.by.expr();
        let cmp = if f.sort.desc { "<" } else { ">" };
        w.push(format!("({col} {cmp} ? OR ({col} = ? AND fi.id {cmp} ?))"));
        if f.sort.by.is_text() {
            let v = c.text.clone().unwrap_or_default();
            p.push(Box::new(v.clone()));
            p.push(Box::new(v));
        } else {
            let v = c.num.unwrap_or(0);
            p.push(Box::new(v));
            p.push(Box::new(v));
        }
        p.push(Box::new(c.id));
    }

    let sql = if w.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", w.join(" AND "))
    };
    (sql, p)
}

/// 최신순 한 페이지. 커서가 None이면 첫 페이지.
pub fn page(
    db: &Db,
    f: &Filter,
    cursor: Option<Cursor>,
    limit: usize,
    group: GroupBy,
) -> Result<Page> {
    let (where_sql, params) = build_where(f, cursor);
    let dir = if f.sort.desc { "DESC" } else { "ASC" };
    let order = format!("{} {dir}, fi.id {dir}", f.sort.by.expr());
    let group_expr = group_expr(group);
    // limit + 1을 읽어 다음 페이지가 있는지 알아낸다
    let sql = format!(
        "SELECT fi.id, fi.name, fi.taken_at, fi.taken_at_source, fi.kind, fi.size,
                fi.width, fi.height, fi.rating, fi.culling_flag, fi.favorite,
                fi.duration_ms, fi.created_at, fi.modified_at, fo.library_id, t.rel_path,
                {group_expr},
                fi.iso, fi.aperture, fi.shutter, fi.focal_mm, fi.cam_model
         FROM files fi
         JOIN folders fo ON fo.id = fi.folder_id
         LEFT JOIN thumbs t ON t.file_id = fi.id AND t.state = 1
         {where_sql}
         ORDER BY {order}
         LIMIT {}",
        limit + 1
    );

    let mut rows = db.read(|c| {
        let mut st = c.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let it = st.query_map(refs.as_slice(), row_to_file)?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;

    let next = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|r| cursor_of(r, f.sort.by))
    } else {
        None
    };
    Ok(Page { rows, next })
}

/// 그룹 머리글에 쓸 값을 SQL로 뽑는다. 프론트는 값이 바뀌는 자리에 줄을 넣는다.
///
/// 서버에서 계산하는 이유: 이어 읽는 페이지의 첫 줄이 앞 페이지의 마지막과
/// 같은 그룹인지 알아야 머리글이 중복되지 않는다. 값이 행에 붙어 있으면
/// 그 비교가 저절로 된다.
fn group_expr(g: GroupBy) -> String {
    match g {
        GroupBy::None => "NULL".into(),
        GroupBy::Folder => "fo.rel_path".into(),
        GroupBy::Day => "date(fi.taken_at,'unixepoch','localtime')".into(),
        GroupBy::Month => "strftime('%Y-%m', fi.taken_at,'unixepoch','localtime')".into(),
        GroupBy::Year => "strftime('%Y', fi.taken_at,'unixepoch','localtime')".into(),
        GroupBy::Rating => "CAST(fi.rating AS TEXT)".into(),
        GroupBy::Camera => "COALESCE(NULLIF(fi.cam_model,''),'(카메라 정보 없음)')".into(),
        GroupBy::Lens => "COALESCE(NULLIF(fi.lens,''),'(렌즈 정보 없음)')".into(),
        GroupBy::FileType => {
            "CASE fi.kind WHEN 0 THEN '사진' WHEN 1 THEN '영상' ELSE 'RAW' END".into()
        }
        GroupBy::Culling => {
            "CASE fi.culling_flag WHEN 1 THEN '남김' WHEN 2 THEN '제외' ELSE '미판정' END".into()
        }
    }
}

/// 마지막 행에서 다음 커서를 만든다. 정렬 기준에 따라 어느 값을 담을지 갈린다.
fn cursor_of(r: &FileRow, by: SortBy) -> Cursor {
    let mut c = Cursor {
        num: None,
        text: None,
        id: r.id,
    };
    match by {
        SortBy::Name => c.text = Some(r.name.clone()),
        SortBy::TakenAt => c.num = Some(r.taken_at),
        SortBy::CreatedAt => c.num = Some(r.created_at.unwrap_or(0)),
        SortBy::ModifiedAt => c.num = Some(r.modified_at.unwrap_or(0)),
        SortBy::Size => c.num = Some(r.size),
        SortBy::Pixels => {
            c.num = Some(r.width.unwrap_or(0) * r.height.unwrap_or(0));
        }
        SortBy::Duration => c.num = Some(r.duration_ms.unwrap_or(0)),
    }
    c
}

/// 타임라인 눈금 하나 — 한 달치.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Bucket {
    pub year: i32,
    pub month: i32,
    pub count: i64,
    /// 이 달에서 가장 최근 촬영 시각. 여기로 점프한다.
    pub top: i64,
}

/// 월별 분포. 우측 스크러버가 쓴다.
///
/// keyset 페이지네이션이라 `top`만 있으면 그 시점부터 바로 읽을 수 있다.
/// OFFSET 방식이었다면 앞의 수만 행을 세고 지나가야 했다.
pub fn timeline(db: &Db, f: &Filter) -> Result<Vec<Bucket>> {
    let (where_sql, params) = build_where(f, None);
    // strftime을 **한 번만** 부른다. '%Y'와 '%m'을 따로 부르면 날짜 계산이
    // 두 번 돈다 (실측 14만 행: 237ms -> 89ms). 쪼개는 건 Rust가 한다.
    let join = if needs_folder_join(f) {
        "JOIN folders fo ON fo.id = fi.folder_id"
    } else {
        ""
    };
    let sql = format!(
        "SELECT strftime('%Y-%m', fi.taken_at, 'unixepoch', 'localtime') ym,
                COUNT(*), MAX(fi.taken_at)
         FROM files fi
         {join}
         {where_sql}
         GROUP BY ym ORDER BY ym DESC"
    );
    db.read(|c| {
        let mut st = c.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let it = st.query_map(refs.as_slice(), |r| {
            let ym: String = r.get(0)?;
            let (y, m) = ym.split_once('-').unwrap_or(("0", "0"));
            Ok(Bucket {
                year: y.parse().unwrap_or(0),
                month: m.parse().unwrap_or(0),
                count: r.get(1)?,
                top: r.get(2)?,
            })
        })?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })
}

/// 전역 순번 `index`에서 페이지를 시작하려면 어떤 커서를 써야 하는가.
///
/// 스크롤바 손잡이를 끌면 "전체의 37% 지점"처럼 **순번**이 나온다. 그런데
/// `page()`는 커서 기반이라 순번을 모른다. 여기서 한 번만 OFFSET으로 그 자리의
/// 행을 찾아 커서로 바꿔 준다. 이후 페이지는 다시 keyset으로 이어 읽는다.
///
/// OFFSET을 쓰지만 `(taken_at DESC, id DESC)` 인덱스만 훑고 테이블은 건드리지
/// 않는다. 6만 행 규모에서 한 번 호출은 밀리초 단위다. 목록 전체를 OFFSET으로
/// 넘기던 옛 방식과는 비용이 다르다.
///
/// `index`가 0 이하면 맨 앞이므로 커서가 없다(None).
pub fn cursor_at(db: &Db, f: &Filter, index: i64) -> Result<Option<Cursor>> {
    if index <= 0 {
        return Ok(None);
    }
    let (where_sql, mut params) = build_where(f, None);
    params.push(Box::new(index - 1));
    let join = if needs_folder_join(f) {
        "JOIN folders fo ON fo.id = fi.folder_id"
    } else {
        ""
    };
    let dir = if f.sort.desc { "DESC" } else { "ASC" };
    let col = f.sort.by.expr();
    let sql = format!(
        "SELECT {col}, fi.id FROM files fi
         {join}
         {where_sql}
         ORDER BY {col} {dir}, fi.id {dir}
         LIMIT 1 OFFSET ?"
    );
    let text = f.sort.by.is_text();
    db.read(|c| {
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        c.query_row(&sql, refs.as_slice(), |r| {
            Ok(if text {
                Cursor {
                    num: None,
                    text: Some(r.get(0)?),
                    id: r.get(1)?,
                }
            } else {
                Cursor {
                    num: Some(r.get(0)?),
                    text: None,
                    id: r.get(1)?,
                }
            })
        })
        .optional()
    })
}

/// 한 행 → FileRow. page()와 by_ids()가 같은 SELECT 열 순서를 쓴다.
fn row_to_file(r: &rusqlite::Row) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: r.get(0)?,
        name: r.get(1)?,
        taken_at: r.get(2)?,
        taken_at_source: r.get(3)?,
        kind: r.get(4)?,
        size: r.get(5)?,
        width: r.get(6)?,
        height: r.get(7)?,
        rating: r.get(8)?,
        culling_flag: r.get(9)?,
        favorite: r.get::<_, i32>(10)? != 0,
        duration_ms: r.get(11)?,
        created_at: r.get(12)?,
        modified_at: r.get(13)?,
        library_id: r.get(14)?,
        thumb: r.get(15)?,
        iso: r.get(17)?,
        aperture: r.get(18)?,
        shutter: r.get(19)?,
        focal_mm: r.get(20)?,
        cam_model: r.get(21)?,
        group: r.get(16)?,
    })
}

/// 주어진 id들의 행 — **주어진 순서대로**. 비슷한 사진처럼 순서가 곧 뜻인 곳에 쓴다.
pub fn by_ids(db: &Db, ids: &[i64]) -> Result<Vec<FileRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT fi.id, fi.name, fi.taken_at, fi.taken_at_source, fi.kind, fi.size,
                fi.width, fi.height, fi.rating, fi.culling_flag, fi.favorite,
                fi.duration_ms, fi.created_at, fi.modified_at, fo.library_id, t.rel_path,
                NULL,
                fi.iso, fi.aperture, fi.shutter, fi.focal_mm, fi.cam_model
         FROM files fi
         JOIN folders fo ON fo.id = fi.folder_id
         LEFT JOIN thumbs t ON t.file_id = fi.id AND t.state = 1
         WHERE fi.id IN ({list})"
    );
    let rows: Vec<FileRow> = db.read(|c| {
        let mut st = c.prepare(&sql)?;
        let it = st.query_map([], row_to_file)?;
        it.collect::<rusqlite::Result<Vec<_>>>()
    })?;
    // IN은 순서를 안 지킨다 — 준 순서로 다시 놓는다
    let by: std::collections::HashMap<i64, FileRow> = rows.into_iter().map(|r| (r.id, r)).collect();
    Ok(ids.iter().filter_map(|id| by.get(id).cloned()).collect())
}

pub fn summary(db: &Db, f: &Filter) -> Result<(i64, i64)> {
    let (where_sql, params) = build_where(f, None);
    let join = if needs_folder_join(f) {
        "JOIN folders fo ON fo.id = fi.folder_id"
    } else {
        ""
    };
    let sql = format!("SELECT COUNT(*), COALESCE(SUM(fi.size),0) FROM files fi {join} {where_sql}");
    db.read(|c| {
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        c.query_row(&sql, refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
    })
}

#[cfg(test)]
mod tests;
