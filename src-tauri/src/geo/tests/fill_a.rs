use super::super::fill::propagate_cached;
use super::super::online::country_code;
use super::*;

// ── [1] 이미 아는 이름이 새 사진에 붙어야 한다 ──────────────────────────

/// 처리한 자리에 새 사진이 들어오면 서버 없이 곧바로 이름이 붙어야 한다.
///
/// 예전에는 오프라인 대상이 «판정이 아예 없는 자리»뿐이라, 캐시가 있는 자리는
/// 화면의 «처리할 곳»이 0 이 되어 단추가 꺼졌다. 서버를 설정하지 않은 사람은
/// 새 사진에 이름을 붙일 길이 아예 없었다 (2026-09-01).
#[test]
fn a_new_photo_in_a_known_place_gets_its_name_without_a_server() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("수원시"));

    // 같은 자리에 새 사진이 들어온다 (스캔이 하는 일)
    db.write(|c| {
            c.execute(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                 VALUES(2,1,'new.jpg',1,0,1,0,0,37.2915,127.0092)",
                [],
            )
        })
        .unwrap();

    // 화면이 «할 일 있음»으로 보여야 단추를 누를 수 있다
    let st = stats(&db).unwrap();
    assert_eq!(st.pending_files, 1, "새 사진이 처리 대기로 잡혀야 한다");
    assert_eq!(st.offline_cells_left, 0, "새로 판정할 자리는 없다");
    assert_eq!(
        st.cache_cells_left, 1,
        "가진 값을 옮기기만 하면 되는 자리가 하나"
    );
    assert!(!st.endpoint_ready, "서버는 설정하지 않았다");

    // 화면이 부르는 바로 그 경로로 처리된다
    let p = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(p.asked, 0, "서버에 묻지 않는다");
    assert_eq!(p.total, 1, "화면이 안내한 수와 실제로 한 일이 같아야 한다");
    assert_eq!(p.files, 1);
    assert_eq!(
        geo_of(&db, 2),
        (
            Some("대한민국".into()),
            Some("경기도".into()),
            Some("수원시".into()),
            Some("수원시".into())
        )
    );
    assert_eq!(stats(&db).unwrap().cache_cells_left, 0);
}

/// 온라인으로 받아 둔 이름도 서버 없이 새 사진에 적용된다
#[test]
fn an_online_name_also_reaches_new_photos_without_a_server() {
    let (_d, db) = db_with(&[(1, 37.5665, 126.9780)]);
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,at)
                   VALUES('37.56,126.97','대한민국','서울특별시','중구','중구','ok','nominatim','remote',0);",
            )
        })
        .unwrap();
    let p = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(p.asked, 0);
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("중구"));

    // 온라인 값이 오프라인 값으로 바뀌지 않았다
    let (source, precision): (String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT source, precision FROM places WHERE cell='37.56,126.97'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(
        (source.as_str(), precision.as_str()),
        (SRC_ONLINE, PREC_REMOTE)
    );
}

/// 화면이 세는 수와 실행이 세는 수는 **같은 질의가 아니다** — 어긋나지 않는지 본다
#[test]
fn the_screen_and_the_run_count_the_same_cells() {
    let (_d, db) = db_with(&[
        (1, 37.2911, 127.0089),
        (2, 37.5665, 126.9780),
        (3, 33.4996, 126.5312),
    ]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    // 세 자리 모두 이름이 있는 상태에서 두 자리에 새 사진을 넣는다
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(4,1,'n1.jpg',1,0,1,0,0,37.2915,127.0092),
                         (5,1,'n2.jpg',1,0,1,0,0,37.5668,126.9783);",
            )
        })
        .unwrap();
    let st = stats(&db).unwrap();
    assert_eq!(st.cache_cells_left, 2);
    assert_eq!(
        st.cache_cells_left,
        cache_cells_left(&db, &valid_gps_sql()).unwrap()
    );
    let p = fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(p.total, 2, "화면이 안내한 수만큼 처리해야 한다");
    assert_eq!(p.files, 2);
    assert_eq!(stats(&db).unwrap().cache_cells_left, 0);
}

/// 라이브러리 범위 전파는 그 라이브러리만 건드린다 —
/// 감시는 폴더가 바뀔 때마다 스캔을 부르므로 헛일을 좁혀야 한다
#[test]
fn a_library_scoped_pass_leaves_other_libraries_alone() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('W','t2','library');
                 INSERT INTO libraries(id,volume_uuid,rel_path,name,area)
                   VALUES(2,'W','b','b',1);
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area,library_id)
                   VALUES(2,'W','b','b',1,2);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(2,2,'other.jpg',1,0,1,0,0,37.2915,127.0092);",
            )
        })
        .unwrap();
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    // 둘 다 같은 자리라 둘 다 붙었다
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("수원시"));
    assert_eq!(geo_of(&db, 2).2.as_deref(), Some("수원시"));

    // 지우고 한쪽만 다시 붙인다
    db.write(|c| {
        c.execute(
            "UPDATE files SET geo_country=NULL, geo_admin1=NULL, geo_admin2=NULL, geo_name=NULL",
            [],
        )
    })
    .unwrap();
    assert_eq!(
        propagate_library(&db, 1).unwrap(),
        1,
        "제 라이브러리만 붙인다"
    );
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("수원시"));
    assert_eq!(geo_of(&db, 2).2, None, "다른 라이브러리는 그대로다");
}

/// 스캔이 끝나면 저절로 붙는다 — 사용자가 단추를 누르지 않아도
#[test]
fn a_scan_applies_what_we_already_know() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    db.write(|c| {
            c.execute(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                 VALUES(2,1,'new.jpg',1,0,1,0,0,37.2915,127.0092)",
                [],
            )
        })
        .unwrap();
    assert_eq!(propagate_cached(&db).unwrap(), 1);
    assert_eq!(geo_of(&db, 2).2.as_deref(), Some("수원시"));
    assert_eq!(propagate_cached(&db).unwrap(), 0, "두 번째는 할 일이 없다");
}

// ── [2] 같은 서버에 같은 좌표를 되풀이해 묻지 않는다 ────────────────────

/// 서버가 «이름 없음»이라 해도 기존 이름은 남고, 두 번째 실행은 묻지 않는다
#[test]
fn a_no_result_is_remembered_so_we_never_ask_that_server_again() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let mut server = TestServer::start(vec![("200 OK", r#"{"error":"Unable to geocode"}"#, None)]);
    set_endpoint(&db, &server.url);

    let first = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(first.asked, 1, "한 번은 물어본다");
    assert_eq!(server.served(), 1);
    assert_eq!(
        geo_of(&db, 1).2.as_deref(),
        Some("수원시"),
        "기존 이름은 그대로다"
    );
    let (status, source, outcome): (String, String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT status, source, online_outcome FROM places WHERE cell='37.29,127.00'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap();
    assert_eq!(status, OK, "값이 살아 있으니 여전히 ok");
    assert_eq!(
        source, SRC_OFFLINE,
        "온라인이 못 찾았다고 출처를 거짓으로 바꾸지 않는다"
    );
    assert_eq!(outcome, ONLINE_NONE);

    // 주소는 그대로 둔다 — 서버는 이미 멈췄으므로, 만약 묻는다면 연결이
    // 실패해 asked 가 올라간다. 요청이 아예 없어야 통과한다.
    assert_eq!(
        stats(&db).unwrap().online_cells_left,
        0,
        "물어볼 곳이 0 이어야 한다"
    );
    let second = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(
        (second.total, second.asked),
        (0, 0),
        "같은 서버에 다시 묻지 않는다"
    );
    assert_eq!(second.stopped, None, "요청이 없으니 멈출 일도 없다");
}

/// 더 얕은 답도 마찬가지 — 값은 지키고, 두 번째 실행은 묻지 않는다
#[test]
fn a_shallow_answer_is_remembered_so_we_never_ask_that_server_again() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"대한민국","country_code":"kr"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);

    let first = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(first.asked, 1);
    assert_eq!(server.served(), 1);
    assert_eq!(
        geo_of(&db, 1).2.as_deref(),
        Some("수원시"),
        "시군구를 잃으면 안 된다"
    );
    let outcome: String = db
        .read(|c| {
            c.query_row(
                "SELECT online_outcome FROM places WHERE cell='37.29,127.00'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(outcome, ONLINE_SHALLOW);

    // 주소를 그대로 둔 채 다시 돌린다 — 물으려 하면 죽은 서버에 걸려 asked 가 오른다
    assert_eq!(stats(&db).unwrap().online_cells_left, 0);
    let second = fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!((second.total, second.asked), (0, 0));
    assert_eq!(second.stopped, None);
}

/// 서버를 바꾸면 다시 물어볼 수 있다 — 다른 서버는 다른 답을 알 수 있다
#[test]
fn changing_the_server_makes_a_settled_cell_worth_asking_again() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let mut first = TestServer::start(vec![("200 OK", r#"{"error":"Unable to geocode"}"#, None)]);
    set_endpoint(&db, &first.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    first.served();
    assert_eq!(stats(&db).unwrap().online_cells_left, 0);

    crate::db::settings::set(&db, "geo.endpoint", "http://other.example/reverse").unwrap();
    assert_eq!(
        stats(&db).unwrap().online_cells_left,
        1,
        "서버가 바뀌면 다시 물어볼 수 있어야 한다"
    );
    assert_eq!(
        targets(
            &db,
            Mode::Online,
            &valid_gps_sql(),
            Some("http://other.example/reverse")
        )
        .unwrap()
        .len(),
        1
    );
}

/// 앱을 껐다 켜도 «물어봤다»는 사실이 남아야 한다
#[test]
fn what_the_server_answered_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = Db::open(&path).unwrap();
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
        let mut server =
            TestServer::start(vec![("200 OK", r#"{"error":"Unable to geocode"}"#, None)]);
        crate::db::settings::set(&db, "geo.endpoint", &server.url).unwrap();
        fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
        server.served();
    }
    // 다시 연다 — 마이그레이션이 한 번 더 돌아도 기록이 지워지면 안 된다
    let db = Db::open(&path).unwrap();
    assert_eq!(stats(&db).unwrap().online_cells_left, 0);
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("수원시"));
}

// ── [3] 경계로 검증한 나라가 온라인 응답에 밀리지 않는다 ──────────────────

/// **독도는 한국 땅이다** — 서버가 더 자세한 일본 주소를 줘도 바뀌지 않는다
#[test]
fn no_server_can_move_dokdo_to_another_country() {
    let (_d, db) = db_with(&[(1, 37.2411, 131.8694)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(geo_of(&db, 1).1.as_deref(), Some("경상북도"));

    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"日本","country_code":"jp","state":"Shimane","city":"Okinoshima"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);

    assert_eq!(
        geo_of(&db, 1),
        (
            Some("대한민국".into()),
            Some("경상북도".into()),
            None,
            Some("경상북도".into())
        ),
        "정책으로 못 박은 좌표가 서버 답에 뒤집혔습니다"
    );
    let outcome: String = db
        .read(|c| {
            c.query_row(
                "SELECT online_outcome FROM places WHERE cell='37.24,131.86'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(outcome, ONLINE_CONFLICT);
    assert_eq!(
        stats(&db).unwrap().online_cells_left,
        0,
        "같은 서버에 다시 묻지 않는다"
    );
}

/// 한국 좌표에 «일본»이라는 답이 오면 기존 값을 지킨다
#[test]
fn a_country_that_disagrees_with_the_boundary_never_wins() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"日本","country_code":"JP","state":"Tokyo","city":"Chiyoda"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);
    assert_eq!(geo_of(&db, 1).0.as_deref(), Some("대한민국"));
    assert_eq!(geo_of(&db, 1).2.as_deref(), Some("수원시"));
}

/// 나라가 맞으면 더 좁은 단위로 제대로 갱신된다 — 이것이 보강의 본래 일이다
#[test]
fn a_matching_country_refines_the_name() {
    let (_d, db) = db_with(&[(1, 37.5665, 126.9780)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(geo_of(&db, 1).1.as_deref(), Some("서울특별시"));

    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"대한민국","country_code":"kr","city":"서울특별시","borough":"중구"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);

    assert_eq!(
        geo_of(&db, 1).2.as_deref(),
        Some("중구"),
        "더 좁은 단위로 갱신돼야 한다"
    );
    let (source, outcome): (String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT source, online_outcome FROM places WHERE cell='37.56,126.97'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!((source.as_str(), outcome.as_str()), (SRC_ONLINE, ONLINE_OK));
}

/// 경계가 나라를 모르는 자리(바다)에서는 서버 답을 그대로 쓴다
#[test]
fn at_sea_the_server_is_the_only_authority() {
    let (_d, db) = db_with(&[(1, 38.5, 131.5)]);
    assert_eq!(
        boundary::country(38.5, 131.5),
        None,
        "이 좌표는 경계가 모르는 곳이어야 한다"
    );
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();

    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"일본","country_code":"jp","state":"Shimane"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);
    assert_eq!(geo_of(&db, 1).0.as_deref(), Some("일본"));
}

/// 나라를 밝히지 않은 답은 경계로 검증된 나라를 바꿀 수 없다
#[test]
fn an_answer_without_a_country_code_cannot_replace_a_verified_country() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"어딘가","state":"어느도","city":"어느시","county":"어느군"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);
    assert_eq!(geo_of(&db, 1).0.as_deref(), Some("대한민국"));
    assert_eq!(stats(&db).unwrap().online_cells_left, 0, "다시 묻지 않는다");
}

/// 국가 코드는 ISO 두 글자만 믿는다 — 이름 글월은 번역 때문에 견줄 수 없다
#[test]
fn only_a_two_letter_code_counts_as_a_country() {
    let cc = |v: &str| country_code(&serde_json::from_str::<serde_json::Value>(v).unwrap());
    assert_eq!(cc(r#"{"country_code":"kr"}"#).as_deref(), Some("KR"));
    assert_eq!(cc(r#"{"country_code":" Jp "}"#).as_deref(), Some("JP"));
    assert_eq!(cc(r#"{"country_code":"gb-eng"}"#), None);
    assert_eq!(cc(r#"{"country_code":""}"#), None);
    assert_eq!(cc(r#"{"country_code":82}"#), None);
    assert_eq!(cc(r#"{"country":"대한민국"}"#), None);
}

/// 서버를 가리키는 이름은 포트·경로까지 보고, 열쇠는 담지 않는다
#[test]
fn a_server_is_identified_by_more_than_its_host() {
    let of = |u: &str| provider_of(Some(u));
    // 포트가 다르면 다른 서버다 — 한 기계에 두 서버를 띄우는 일은 흔하다
    assert_ne!(
        of("http://127.0.0.1:8080/reverse"),
        of("http://127.0.0.1:9090/reverse")
    );
    // 경로가 다르면 다른 서버다
    assert_ne!(of("http://a.example/one"), of("http://a.example/two"));
    // 대소문자와 끝 빗금은 같은 것으로 본다
    assert_eq!(
        of("http://A.Example/reverse/"),
        of("http://a.example/reverse")
    );
    // 기본 포트를 적었든 안 적었든 같다
    assert_eq!(
        of("http://a.example:80/reverse"),
        of("http://a.example/reverse")
    );
    assert_eq!(
        of("https://a.example:443/reverse"),
        of("https://a.example/reverse")
    );
    // scheme 이 다르면 다른 서버다
    assert_ne!(
        of("http://a.example/reverse"),
        of("https://a.example/reverse")
    );
    // **열쇠는 담지 않는다**
    let with_key = of("http://a.example/reverse?key=s3cr3t").unwrap();
    assert!(!with_key.contains("s3cr3t"), "{with_key}");
    assert_eq!(Some(with_key), of("http://a.example/reverse"));
    // 쓸 수 없는 주소는 이름도 없다
    assert_eq!(of("https://nominatim.openstreetmap.org/reverse"), None);
    assert_eq!(of("그냥 글자"), None);
    assert_eq!(provider_of(None), None);
}

/// 판정 규칙만 따로 — 경계가 이기고, 얕은 답은 물러난다
#[test]
fn the_boundary_decides_before_the_depth_does() {
    let j = |b, a, ok, nd, od| judge(b, a, ok, nd, od);
    assert_eq!(
        j(Some("KR"), Some("JP"), None, 3, 1),
        Verdict::Conflict,
        "더 깊어도 나라가 다르면 진다"
    );
    assert_eq!(
        j(Some("KR"), Some("kr"), None, 3, 2),
        Verdict::Accept,
        "대소문자는 상관없다"
    );
    assert_eq!(
        j(Some("KR"), None, None, 3, 1),
        Verdict::Shallow,
        "나라를 안 밝히면 못 바꾼다"
    );
    assert_eq!(
        j(None, None, None, 3, 1),
        Verdict::Accept,
        "경계가 모르면 서버를 믿는다"
    );
    assert_eq!(
        j(None, Some("JP"), None, 1, 3),
        Verdict::Shallow,
        "얕아지면 물러난다"
    );
    assert_eq!(
        j(Some("KR"), Some("KR"), None, 2, 2),
        Verdict::Accept,
        "같은 깊이는 받아들인다"
    );
    // 나라가 같아도 도가 어긋나면 막는다
    assert_eq!(
        j(Some("KR"), Some("KR"), Some(false), 3, 1),
        Verdict::Conflict,
        "도가 틀리면 진다"
    );
    assert_eq!(
        j(Some("KR"), Some("KR"), Some(true), 3, 2),
        Verdict::Accept,
        "도가 맞으면 받아들인다"
    );
    // 모르는 시도 이름에는 다투지 않는다 — 그러면 정상 응답까지 막힌다
    assert_eq!(j(Some("KR"), Some("KR"), None, 3, 2), Verdict::Accept);
}

/// **나라가 같아도 도가 다르면 기존 경계 판정을 지킨다.**
///
/// 격자 대표 좌표는 칸 안 어딘가일 뿐이라, 도 경계에 걸친 칸에서 서버가 옆
/// 도를 답하는 일이 실제로 생긴다 (2026-09-01).
#[test]
fn a_wrong_province_in_the_right_country_never_wins() {
    let (_d, db) = db_with(&[(1, 37.2911, 127.0089)]);
    fill_offline(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(geo_of(&db, 1).1.as_deref(), Some("경기도"));

    // 나라는 맞지만 도가 틀린 답 — 시군구까지 있어 «더 깊다»
    let mut server = TestServer::start(vec![(
        "200 OK",
        r#"{"address":{"country":"대한민국","country_code":"kr","state":"경상북도","city":"경주시"}}"#,
        None,
    )]);
    set_endpoint(&db, &server.url);
    fill(&db, &AtomicBool::new(false), None, |_| {}).unwrap();
    assert_eq!(server.served(), 1);

    assert_eq!(
        geo_of(&db, 1),
        (
            Some("대한민국".into()),
            Some("경기도".into()),
            Some("수원시".into()),
            Some("수원시".into())
        ),
        "경계가 정한 도가 서버 답에 밀렸습니다"
    );
    let outcome: String = db
        .read(|c| {
            c.query_row(
                "SELECT online_outcome FROM places WHERE cell='37.29,127.00'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(outcome, ONLINE_CONFLICT);
    assert_eq!(
        stats(&db).unwrap().online_cells_left,
        0,
        "같은 서버에 다시 묻지 않는다"
    );
}
