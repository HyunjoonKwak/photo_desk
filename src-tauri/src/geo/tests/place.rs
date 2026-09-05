use super::*;

#[test]
fn a_cell_is_a_hundredth_of_a_degree_and_negatives_floor_the_same_way() {
    assert_eq!(cell(37.2846, 127.0512), "37.28,127.05");
    assert_eq!(cell(37.2899, 127.0599), "37.28,127.05", "같은 칸");
    assert_eq!(cell(-33.8688, 151.2093), "-33.87,151.20");
    let (lat, lon) = cell_center("37.28,127.05").unwrap();
    assert_eq!(
        cell(lat, lon),
        "37.28,127.05",
        "가운데 점은 제 칸으로 돌아온다"
    );
}

/// 서울은 state 가 없고 city·borough 로 온다 — 시도로 승격해야 한다
#[test]
fn seoul_promotes_the_city_to_the_first_level() {
    let p = fold(&json!({"borough": "서초구", "city": "서울특별시", "country": "대한민국"}));
    assert_eq!(
        p,
        Place {
            country: Some("대한민국".into()),
            admin1: Some("서울특별시".into()),
            admin2: Some("서초구".into())
        }
    );
    assert_eq!(p.name().as_deref(), Some("서초구"));
}

#[test]
fn a_province_and_a_city_map_straight_through() {
    let p = fold(&json!({"province": "경기도", "city": "수원시", "country": "대한민국"}));
    assert_eq!(p.admin1.as_deref(), Some("경기도"));
    assert_eq!(p.admin2.as_deref(), Some("수원시"));
}

#[test]
fn overseas_uses_state_and_city() {
    let p =
        fold(&json!({"state": "뉴사우스웨일스주", "city": "시드니", "country": "오스트레일리아"}));
    assert_eq!(p.admin1.as_deref(), Some("뉴사우스웨일스주"));
    assert_eq!(p.admin2.as_deref(), Some("시드니"));
}

/// 같은 이름이 두 칸에 겹쳐 와도 두 단계에 같은 글자를 넣지 않는다
#[test]
fn a_duplicated_name_is_not_repeated_across_levels() {
    let p = fold(
        &json!({"province": "제주특별자치도", "city": "제주특별자치도", "county": "서귀포시"}),
    );
    assert_eq!(p.admin1.as_deref(), Some("제주특별자치도"));
    assert_eq!(p.admin2.as_deref(), Some("서귀포시"));
}

#[test]
fn an_empty_address_yields_nothing_to_show() {
    let p = fold(&json!({}));
    assert!(p.is_empty());
    assert_eq!(p.name(), None);
}

#[test]
fn the_public_batch_endpoint_is_refused() {
    assert!(validate_endpoint("https://nominatim.openstreetmap.org/reverse").is_err());
    assert!(validate_endpoint("https://nominatim.openstreetmap.org./reverse").is_err());
    assert!(validate_endpoint("http://127.0.0.1:8080/reverse").is_ok());
}

/// SQL 격자 식과 러스트 cell() 이 같은 칸을 가리켜야 한다 — 음수 좌표 포함
#[test]
fn the_sql_grid_matches_the_rust_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    for (lat, lon) in [
        (37.2846, 127.0512),
        (-33.8688, 151.2093),
        (21.3, -157.86),
        (0.005, -0.005),
    ] {
        let got: String = db
            .read(|c| {
                c.query_row(
                    &format!("SELECT {}", cell_sql(&lat.to_string(), &lon.to_string())),
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(got, cell(lat, lon), "{lat},{lon}");
    }
}

/// 이름이 필요한 격자만 센다 — 이미 이름이 있으면 세지 않는다
#[test]
fn stats_count_only_what_still_needs_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2846,127.0512),
                         (2,1,'b.jpg',1,0,1,0,0,37.2899,127.0599),
                         (3,1,'c.jpg',1,0,1,0,0,-33.8688,151.2093),
                         (4,1,'d.jpg',1,0,1,0,0,NULL,NULL),
                         (5,1,'bad-lat.jpg',1,0,1,0,0,200.0,127.0),
                         (6,1,'no-lon.jpg',1,0,1,0,0,37.0,NULL),
                         (7,1,'null-island.jpg',1,0,1,0,0,0.0,0.0);",
            )
        })
        .unwrap();
    let s = stats(&db).unwrap();
    assert_eq!(
        (
            s.with_gps,
            s.named,
            s.pending_files,
            s.unavailable_files,
            s.cells_left
        ),
        (3, 0, 3, 0, 2),
        "같은 칸 둘은 한 번만 세고 잘못된 좌표는 대상에서 뺀다"
    );
    // 아직 아무 판정이 없는 자리는 오프라인으로 풀 수 있다 — 서버가 필요 없다
    assert_eq!((s.offline_cells_left, s.network_cells_left), (2, 0));
    assert!(!s.endpoint_ready, "기본값으로 공개 배치 서버를 쓰지 않는다");

    db.write(|c| {
        c.execute(
            "UPDATE files SET geo_country='대한민국' WHERE id IN (1,2)",
            [],
        )
    })
    .unwrap();
    let s = stats(&db).unwrap();
    assert_eq!(
        (s.named, s.pending_files, s.cells_left, s.offline_cells_left),
        (2, 1, 1, 1)
    );
}
