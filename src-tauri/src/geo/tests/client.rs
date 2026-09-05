use super::*;

/// 서버 주소에 열쇠가 들어 있으면 오류 글월을 타고 로그에 남는다 — 떼어 낸다.
///
/// 자체 Nominatim 을 `?key=...` 로 지키는 구성이 흔하다. 연결이 실패했을 뿐인데
/// 그 열쇠가 로그 파일에 남으면 안 된다.
#[test]
fn a_failure_never_leaks_the_key_in_the_server_address() {
    // 아무도 듣지 않는 자리 — 반드시 연결에 실패한다
    let secret = "s3cr3t-token-do-not-log";
    let dead = format!("http://127.0.0.1:1/reverse?key={secret}");
    let answer = ask(&test_client(), &dead, 37.5, 127.0, 12);
    match answer {
        Answer::Retryable { message, .. } => {
            assert!(
                !message.contains(secret),
                "열쇠가 오류 글월에 남았습니다: {message}"
            );
            assert!(
                !message.contains("127.0.0.1"),
                "주소가 오류 글월에 남았습니다: {message}"
            );
            assert!(
                message.contains("연결하지 못했습니다"),
                "무슨 일인지는 말해야 한다: {message}"
            );
        }
        other => panic!(
            "다시 물어볼 답이어야 한다: {}",
            matches!(other, Answer::Fatal(_))
        ),
    }
}

/// 잠깐 흔들린 서버에는 다시 물어본다 — 한 번 실패했다고 20분 작업을 버리지 않는다
#[test]
fn a_shaky_server_gets_another_chance() {
    let mut server = TestServer::start(vec![
        // 첫 답은 «곧 다시 와도 된다» — 그래도 초당 한 건은 지킨다
        ("503 Service Unavailable", r#"{}"#, Some("Retry-After: 0")),
        (
            "200 OK",
            r#"{"address":{"country":"대한민국","state":"경기도","city":"수원시"}}"#,
            None,
        ),
    ]);
    let began = std::time::Instant::now();
    let answer = ask_with_retry(
        &test_client(),
        &server.url,
        37.5,
        127.0,
        12,
        &AtomicBool::new(false),
    );
    let waited = began.elapsed();
    assert_eq!(server.served(), 2, "두 번 물어봐야 한다");
    match answer {
        Answer::Found(f) => assert_eq!(f.place.admin2.as_deref(), Some("수원시")),
        _ => panic!("두 번째 답을 받아야 한다"),
    }
    assert!(
        waited >= GAP,
        "재시도가 초당 한 건 약속을 깨면 안 된다 — {waited:?}"
    );
}

/// 세 번을 다 쓰고도 안 되면 멈춘다 — 끝없이 두드리지 않는다
#[test]
fn a_dead_server_is_not_hammered_forever() {
    let cancel = AtomicBool::new(false);
    let mut server = TestServer::start(vec![(
        "503 Service Unavailable",
        r#"{}"#,
        Some("Retry-After: 0"),
    )]);
    let answer = ask_with_retry(&test_client(), &server.url, 37.5, 127.0, 12, &cancel);
    assert_eq!(server.served(), RETRIES.len(), "정해진 횟수만 물어본다");
    assert!(matches!(answer, Answer::Retryable { .. }));
}

/// 백오프 중에 «그만»을 누르면 곧바로 멈춘다 — 15초를 다 기다리지 않는다
#[test]
fn stopping_during_a_backoff_takes_effect_at_once() {
    let cancel = AtomicBool::new(true);
    let began = std::time::Instant::now();
    assert!(!nap(&cancel, Duration::from_secs(15)));
    assert!(began.elapsed() < Duration::from_secs(1));
}

/// 서버가 못 찾았다고 해서 이미 붙은 이름을 지우면 안 된다
#[test]
fn a_server_that_finds_nothing_never_erases_a_name() {
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

    let settled = settle_empty(&db, "37.29,127.00", NONE, SRC_ONLINE, Some("my.server")).unwrap();
    assert!(!settled, "이미 이름이 있으면 못 박지 않는다");

    let (country, status): (Option<String>, String) = db
        .read(|c| {
            c.query_row(
                "SELECT country, status FROM places WHERE cell='37.29,127.00'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(country.as_deref(), Some("대한민국"), "이름이 지워졌습니다");
    assert_eq!(status, OK);

    // 이름이 없는 자리는 정상적으로 못 박는다
    assert!(settle_empty(&db, "10.00,10.00", NONE, SRC_ONLINE, None).unwrap());
}

/// 서버 답이 기존보다 얕으면 그대로 둔다 — 시군구까지 있는 자리가 나라만 남으면 후퇴다
#[test]
fn a_shallower_answer_never_replaces_a_deeper_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO places(cell,country,admin1,admin2,name,status,source,precision,at)
                   VALUES('1,1','대한민국','경기도','수원시','수원시','ok','offline_geonames','approximate',0);",
            )
        })
        .unwrap();
    assert_eq!(current_depth(&db, "1,1").unwrap(), 3);
    assert_eq!(
        current_depth(&db, "9,9").unwrap(),
        0,
        "없는 자리는 0이라 무엇이든 들어간다"
    );

    let only_country = Place {
        country: Some("대한민국".into()),
        ..Default::default()
    };
    assert!(only_country.depth() < current_depth(&db, "1,1").unwrap());
    // 위가 비면 아래도 세지 않는다
    assert_eq!(
        Place {
            admin2: Some("수원시".into()),
            ..Default::default()
        }
        .depth(),
        0
    );
}

/// 사용자가 멈춘 것도 결과에 적는다 — 안 그러면 화면이 «다 했습니다»라고 한다
#[test]
fn stopping_is_reported_as_a_stop_not_a_success() {
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
    let p = fill_offline(&db, &AtomicBool::new(true), None, |_| {}).unwrap();
    assert_eq!(p.total, 1, "할 일은 있었다");
    assert_eq!(p.done, 0);
    assert_eq!(p.stopped.as_deref(), Some("멈췄습니다"));
    assert!(
        p.cancelled,
        "사용자가 멈춘 것과 서버가 막은 것은 다르게 보여야 한다"
    );
}

/// 지도가 세는 사진과 지명이 처리하는 사진은 **같은 사진**이어야 한다.
///
/// 좌표 조건이 두 곳에 따로 적혀 있던 시절, 한쪽만 고치면 지도에는 보이는데
/// 지명은 영영 «처리할 수 없는» 사진이 생겼다. 경계값을 한 상 차려 두고
/// 두 숫자가 같은지 본다.
#[test]
fn the_map_and_the_place_names_count_the_same_photos() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    let coords: &[(Option<f64>, Option<f64>)] = &[
        (Some(37.5), Some(127.0)),
        (Some(90.0), Some(180.0)),
        (Some(-90.0), Some(-180.0)),
        (Some(0.0), Some(127.0)),
        (Some(37.5), Some(0.0)),
        (Some(0.0), Some(0.0)),
        (None, Some(127.0)),
        (Some(37.5), None),
        (Some(90.1), Some(127.0)),
        (Some(37.5), Some(180.1)),
        (None, None),
    ];
    db.write(|c| {
        c.execute_batch(
            "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);",
        )
    })
    .unwrap();
    for (i, (lat, lon)) in coords.iter().enumerate() {
        let id = i as i64 + 1;
        db.write(|c| {
                c.execute(
                    "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                     VALUES(?1,1,?2,1,0,1,0,0,?3,?4)",
                    rusqlite::params![id, format!("f{id}.jpg"), lat, lon],
                )
            })
            .unwrap();
    }
    // 휴지통에 든 사진은 어느 쪽도 세지 않는다
    db.write(|c| {
            c.execute(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon,trashed_at)
                 VALUES(99,1,'gone.jpg',1,0,1,0,0,37.5,127.0,1)",
                [],
            )
        })
        .unwrap();

    let by_map = crate::db::query::map_overview(&db, &crate::db::query::Filter::default()).unwrap();
    let by_geo = stats(&db).unwrap();
    assert_eq!(
        by_geo.with_gps, by_map.total,
        "지도와 지명이 다른 사진을 셉니다"
    );
    assert_eq!(by_geo.with_gps, 5, "경계값을 포함해 다섯 장이 유효하다");
    // 셀 수 있는 것은 모두 처리할 수 있어야 한다 — 세기만 하고 못 붙이는 사진이 없게
    assert_eq!(by_geo.pending_files, by_geo.with_gps);
    let cells = targets(&db, Mode::Offline, &valid_gps_sql(), None)
        .unwrap()
        .len() as i64;
    assert_eq!(
        cells, by_geo.offline_cells_left,
        "처리할 자리 수도 같아야 한다"
    );
}
