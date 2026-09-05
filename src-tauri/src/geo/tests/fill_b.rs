use super::*;

/// 영문으로 답해도 같은 판정이어야 한다 — 표기 차이로 막히면 안 된다
#[test]
fn an_english_province_name_is_understood_too() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"South Korea","country_code":"kr","state":"Gyeonggi-do","city":"Suwon-si","borough":"Yeongtong-gu"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);
    // 도가 맞으므로 받아들인다. 시군구는 fold 의 차례대로 city 가 먼저다.
    assert_eq!(geo_of(&db, 1).1.as_deref(), Some("Gyeonggi-do"));
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("Suwon-si"));
}

/// 서버 없이 채운다 — 내장 자료만으로 세 단계가 다 붙는다
#[test]
fn the_offline_pass_names_photos_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'suwon.jpg',1,0,1,0,0,37.2911,127.0089),
                         (2,1,'suwon2.jpg',1,0,1,0,0,37.2915,127.0092),
                         (3,1,'dokdo.jpg',1,0,1,0,0,37.2411,131.8694),
                         (4,1,'sea.jpg',1,0,1,0,0,38.5,131.5);",
            )
        })
        .unwrap();

    let p = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(p.asked, 0, "서버에 한 번도 묻지 않는다");
    assert_eq!(
        (p.total, p.done, p.files, p.empty),
        (3, 3, 3, 1),
        "바다 한 자리는 못 정한다"
    );

    let named = |id: i64| -> (Option<String>, Option<String>, Option<String>) {
        db.read(|c| {
            c.query_row(
                "SELECT geo_country, geo_admin1, geo_admin2 FROM files WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap()
    };
    assert_eq!(
        named(1),
        (
            Some("대한민국".into()),
            Some("경기도".into()),
            Some("수원시".into())
        )
    );
    assert_eq!(
        named(2).2,
        Some("수원시".into()),
        "같은 자리의 다른 사진에도 붙는다"
    );
    // **독도는 한국 땅이다** — 채우기 전체를 지나온 뒤에도 그렇다
    assert_eq!(
        named(3),
        (Some("대한민국".into()), Some("경상북도".into()), None)
    );
    assert_eq!(named(4), (None, None, None), "바다는 온라인 몫으로 남는다");

    // 못 정한 자리는 «다시 물어볼 수 있음»으로 남는다 — 못 박지 않는다
    let (status, source): (String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT status, source FROM places WHERE country IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(
        (status.as_str(), source.as_str()),
        (UNRESOLVED, SRC_OFFLINE)
    );

    // 판을 캐시에 적어 둔다 — 나중에 어느 자료로 붙였는지 알 수 있게
    let version: Option<String> = db
        .read(|c| {
            c.query_row(
                "SELECT dataset_version FROM places WHERE cell LIKE '37.29%'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(version.as_deref(), Some(offline::dataset_version()));

    // 다시 돌려도 할 일이 없다 — 같은 자리를 두 번 판정하지 않는다
    let again = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(again.total, 0);
}

/// 오프라인이 채운 자리는 온라인 보강 대상으로 남는다 — 캐시로 오해하면 영영 근사값이다
#[test]
fn an_offline_result_still_waits_for_the_online_pass() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2911,127.0089);",
            )
        })
        .unwrap();
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();

    let s = stats(&db).unwrap();
    assert_eq!((s.named, s.approximate_files, s.precise_files), (1, 1, 0));
    assert_eq!(s.pending_files, 0, "이름은 붙었으니 «처리할 사진»은 아니다");
    assert_eq!(
        targets(&db, Mode::Offline, &valid_gps_sql(), None)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        targets(&db, Mode::Online, &valid_gps_sql(), None)
            .unwrap()
            .len(),
        1,
        "정밀 보강 대상이다"
    );
    // 화면이 세는 수와 실제로 처리할 자리 수가 같아야 한다
    assert_eq!(s.offline_cells_left, 0);
    assert_eq!(s.online_cells_left, 1);
}

/// 서버가 «이름 없음»이라 한 자리는 **그 서버에는** 다시 묻지 않는다.
/// 오프라인도 건드리지 않는다 — 값이 없다고 내장 자료로 지어내지 않는다.
#[test]
fn a_settled_empty_cell_is_left_alone_by_the_same_server() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2911,127.0089);
                 INSERT INTO places(cell,status,source,precision,
                                    online_outcome,online_provider,online_checked_at,at)
                   VALUES('37.29,127.00','none','nominatim','remote','none','http://a.example/reverse',0,0);",
            )
        })
        .unwrap();
    assert_eq!(
        targets(&db, Mode::Offline, &valid_gps_sql(), None)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        targets(
            &db,
            Mode::Online,
            &valid_gps_sql(),
            Some("http://a.example/reverse")
        )
        .unwrap()
        .len(),
        0,
        "같은 서버에는 다시 묻지 않는다"
    );
    let p = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(p.total, 0, "오프라인이 서버의 확정을 뒤집으면 안 된다");

    // 서버를 바꾸면 다시 물어볼 수 있다 — «없다»는 그 서버의 답이었을 뿐이다
    assert_eq!(
        targets(
            &db,
            Mode::Online,
            &valid_gps_sql(),
            Some("http://b.example/reverse")
        )
        .unwrap()
        .len(),
        1,
        "다른 서버는 알 수도 있다"
    );
}

/// **서버 A 가 «없다»고 한 자리를 서버 B 로 바꾸면 다시 조회한다.**
///
/// «이름이 없다»는 그 서버의 답이지 세상의 사실이 아니다. 자체 Nominatim 의
/// 지역 자료가 좁아 못 찾은 것을 다른 서버는 알 수도 있다 (2026-09-01).
#[test]
fn a_new_server_gets_to_answer_a_cell_the_old_one_gave_up_on() {
    // 내장 경계가 나라를 모르는 자리라야 서버 답이 그대로 쓰인다 —
    // 육지였다면 국가 충돌로 거부되어 이 시험의 뜻이 흐려진다
    let (_d, db) = db_with(&[(1, 0.005, -140.005)]);
    assert_eq!(boundary::country(0.005, -140.005), None);
    let mut a = TestServer::start(vec![("200 OK", r#"{"error":"Unable to geocode"}"#, None)]);
    set_endpoint(&db, &a.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(a.served(), 1);
    let status: String = db
        .read(|c| {
            c.query_row(
                "SELECT status FROM places WHERE cell='0.00,-140.01'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(status, NONE, "이름이 없다고 못 박혔다");
    assert_eq!(
        stats(&db).unwrap().online_cells_left,
        0,
        "그 서버에는 더 물을 것이 없다"
    );

    // 서버를 바꾼다 — 이번엔 답을 안다
    let mut b = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"어느나라","country_code":"xx","state":"어느주","city":"어느시"}}"#,
        None,
    )]);
    set_endpoint(&db, &b.url);
    assert_eq!(
        stats(&db).unwrap().online_cells_left,
        1,
        "새 서버에는 물어볼 곳이 있다"
    );

    let p = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(b.served(), 1, "새 서버에 물어봐야 한다");
    assert_eq!(p.files, 1);
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("어느시"));
    assert_eq!(stats(&db).unwrap().online_cells_left, 0);
}

/// 캐시에 있으면 묻지 않고 곧바로 사진에 붙인다 — 네트워크 없이 도는 길
#[test]
fn a_cached_cell_names_its_photos_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2846,127.0512),
                         (2,1,'b.jpg',1,0,1,0,0,37.2899,127.0599);
                 INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,at)
                   VALUES('37.28,127.05','대한민국','경기도','수원시','수원시','ok','nominatim','remote',0);",
            )
        })
        .unwrap();

    let before = stats(&db).unwrap();
    assert_eq!(
        (
            before.cells_left,
            before.offline_cells_left,
            before.network_cells_left
        ),
        (1, 0, 0),
        "성공 캐시가 있는 자리는 조회 없이 붙이기만 하면 된다"
    );

    let p = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(
        (p.total, p.asked, p.files),
        (1, 0, 2),
        "묻지 않고 두 장에 붙는다"
    );

    let (c1, a2, name): (String, String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT geo_country, geo_admin2, geo_name FROM files WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap();
    assert_eq!(
        (c1.as_str(), a2.as_str(), name.as_str()),
        ("대한민국", "수원시", "수원시")
    );

    // 두 번째로 부르면 할 일이 없다
    let again = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(again.total, 0);
}

#[test]
fn an_uncached_fill_requires_an_allowed_batch_server() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2846,127.0512);",
            )
        })
        .unwrap();

    let err = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap_err();
    assert!(err.to_string().contains("지명 서버를 먼저"));

    crate::db::settings::set(
        &db,
        ENDPOINT_KEY,
        "https://nominatim.openstreetmap.org/reverse",
    )
    .unwrap();
    let err = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap_err();
    assert!(err.to_string().contains("공개 Nominatim"));
}

/// 이름이 없는 자리(status='none')는 두 번 묻지 않고, 남은 곳 셈에서도 빠진다.
/// 전에는 빈 캐시를 «성공»으로 읽어 매번 같은 칸을 다시 대상으로 삼고
/// «N장에 붙였습니다»까지 거짓으로 셌다 (2026-09-01 리뷰)
#[test]
fn a_place_with_no_name_is_not_asked_again_and_is_not_counted_as_named() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,10.005,20.005),
                         (2,1,'b.jpg',1,0,1,0,0,10.006,20.006);
                 INSERT INTO places(cell,country,admin1,admin2,name,status,
                                    online_outcome,online_provider,at)
                   VALUES('10.00,20.00',NULL,NULL,NULL,NULL,'none','none','http://my.server/reverse',0);",
            )
        })
        .unwrap();

    // 물어볼 것이 없다 — 네트워크를 건드리지 않는다
    let p = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(
        (p.total, p.asked, p.files),
        (0, 0, 0),
        "이름 없는 자리는 대상이 아니다"
    );

    // 남은 곳 셈에서도 빠진다 — 전에는 영영 줄지 않았다
    let s = stats(&db).unwrap();
    assert_eq!(
        (
            s.with_gps,
            s.named,
            s.pending_files,
            s.unavailable_files,
            s.cells_left
        ),
        (2, 0, 0, 2, 0)
    );
    assert_eq!((s.offline_cells_left, s.network_cells_left), (0, 0));
}

/// unresolved 는 «다시 물을 수 있는 것»이라 none 과 달리 대상에 남는다
#[test]
fn an_unresolved_cell_stays_available_for_the_online_pass() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2846,127.0512);
                 INSERT INTO places(cell,status,source,at)
                   VALUES('37.28,127.05','unresolved','offline_geonames',0);",
            )
        })
        .unwrap();
    let s = stats(&db).unwrap();
    assert_eq!(s.pending_files, 1, "아직 처리할 수 있는 사진이다");
    assert_eq!(s.unavailable_files, 0, "«서버에도 없음»이 아니다");
    assert_eq!(
        (s.offline_cells_left, s.network_cells_left),
        (0, 1),
        "오프라인은 포기했고 서버만 남았다"
    );
}
