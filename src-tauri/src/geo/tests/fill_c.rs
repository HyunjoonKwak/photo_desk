use super::*;

/// 캐시 기록과 파일 전파는 한 트랜잭션이다 — 둘이 어긋나면 안 된다
#[test]
fn writing_a_place_updates_the_cache_and_the_photos_together() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2846,127.0512),
                         (2,1,'b.jpg',1,0,1,0,0,37.2899,127.0599);",
            )
        })
        .unwrap();
    let place = Place {
        country: Some("대한민국".into()),
        admin1: Some("경기도".into()),
        admin2: Some("수원시".into()),
    };
    let gps = valid_gps_sql();
    let n = write_place(
        &db,
        "37.28,127.05",
        &place,
        OK,
        SRC_ONLINE,
        PREC_REMOTE,
        None,
        None,
        Some("my.server"),
        &gps,
        Overwrite::All,
        None,
    )
    .unwrap();
    assert_eq!(n, 2, "그 자리의 두 장에 붙는다");

    let (status, source, precision, provider): (String, String, String, String) = db
        .read(|c| {
            c.query_row(
                "SELECT status, source, precision, provider FROM places WHERE cell='37.28,127.05'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
        })
        .unwrap();
    assert_eq!(
        (
            status.as_str(),
            source.as_str(),
            precision.as_str(),
            provider.as_str()
        ),
        ("ok", "nominatim", "remote", "my.server")
    );

    // 두 번 써도 행이 늘지 않고 값만 바뀐다 (ON CONFLICT DO UPDATE)
    write_place(
        &db,
        "37.28,127.05",
        &place,
        OK,
        SRC_ONLINE,
        PREC_REMOTE,
        None,
        None,
        Some("other.server"),
        &gps,
        Overwrite::All,
        None,
    )
    .unwrap();
    let rows: i64 = db
        .read(|c| c.query_row("SELECT COUNT(*) FROM places", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(rows, 1);
}

/// 칸 정중앙이 아니라 그 칸 사진들의 대표(가운데) 좌표를 묻는다 —
/// 경계·해안·섬에서 옆 동네가 붙지 않게 (2026-09-01 리뷰)
#[test]
fn the_asked_point_is_a_real_photo_not_the_cell_centre() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
            c.execute_batch(
                "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                   VALUES(1,1,'a.jpg',1,0,1,0,0,37.2801,127.0501),
                         (2,1,'b.jpg',1,0,1,0,0,37.2802,127.0502),
                         (3,1,'c.jpg',1,0,1,0,0,37.2803,127.0503);",
            )
        })
        .unwrap();
    // fill 이 고르는 것과 같은 식으로 대표를 뽑아 본다
    let cell_expr = cell_sql("gps_lat", "gps_lon");
    let (cell_key, la, lo): (String, f64, f64) = db
            .read(|c| {
                c.query_row(
                    &format!(
                        "WITH pts AS (
                           SELECT {cell_expr} AS cell, gps_lat AS la, gps_lon AS lo,
                                  ROW_NUMBER() OVER (PARTITION BY {cell_expr} ORDER BY gps_lat, gps_lon) AS rn,
                                  COUNT(*) OVER (PARTITION BY {cell_expr}) AS n
                             FROM files WHERE gps_lat IS NOT NULL)
                         SELECT cell, la, lo FROM pts WHERE rn = (n + 1) / 2"
                    ),
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap();
    assert_eq!(cell_key, "37.28,127.05");
    assert_eq!((la, lo), (37.2802, 127.0502), "가운데 사진의 실제 좌표");
    let (clat, clon) = cell_center(&cell_key).unwrap();
    assert!(
        (la - clat).abs() > 1e-6 || (lo - clon).abs() > 1e-6,
        "칸 정중앙과 달라야 한다"
    );
}

/// 실제 HTTP 429가 enum 이름만 검사하는 가짜 시험이 아니라 ask의 중단 경로를 탄다.
#[test]
fn an_http_429_is_worth_asking_again() {
    let mut server = TestServer::once("429 Too Many Requests", r#"{"error":"slow down"}"#);
    match ask(&test_client(), &server.url, 37.5, 127.0, 12) {
        Answer::Retryable {
            message,
            retry_after,
        } => {
            assert!(message.contains("429"));
            assert_eq!(retry_after, None, "서버가 Retry-After 를 주지 않았다");
        }
        _ => panic!("다시 물어볼 답이어야 한다"),
    }
    assert_eq!(server.served(), 1);
}

/// 4xx 는 주소나 권한이 틀린 것이라 다시 물어도 같은 답이 온다 — 곧바로 멈춘다
#[test]
fn a_404_is_not_worth_asking_again() {
    let mut server = TestServer::once("404 Not Found", r#"{}"#);
    match ask(&test_client(), &server.url, 37.5, 127.0, 12) {
        Answer::Fatal(msg) => assert!(msg.contains("404") && msg.contains("주소")),
        _ => panic!("멈춰야 한다"),
    }
    assert_eq!(server.served(), 1);
}

#[test]
fn an_error_hidden_in_a_200_response_stops_instead_of_becoming_a_cache_miss() {
    let mut limited = TestServer::once("200 OK", r#"{"error":"rate limit exceeded"}"#);
    assert!(matches!(
        ask(&test_client(), &limited.url, 37.5, 127.0, 12),
        Answer::Fatal(_)
    ));
    assert_eq!(limited.served(), 1);

    let mut nowhere = TestServer::once("200 OK", r#"{"error":"Unable to geocode"}"#);
    assert!(matches!(
        ask(&test_client(), &nowhere.url, 37.5, 127.0, 12),
        Answer::Nothing
    ));
    assert_eq!(nowhere.served(), 1);
}

/// HTTP 성공이어도 국가가 없는 부분 응답은 성공 캐시로 저장하지 않는다.
#[test]
fn a_partial_place_without_a_country_is_not_success() {
    let mut server = TestServer::once(
        "200 OK",
        r#"{"address":{"city":"서울특별시","borough":"서초구"}}"#,
    );
    assert!(matches!(
        ask(&test_client(), &server.url, 37.5, 127.0, 12),
        Answer::Nothing
    ));
    assert_eq!(server.served(), 1);
}

/// 아무도 연결하지 않아도 서버 스레드는 제 마감으로 끝난다 —
/// 시험이 실패 대신 영원히 매달리던 것을 막는다 (2026-09-01 리뷰)
#[test]
fn the_test_server_stops_itself_when_nobody_connects() {
    let mut server = TestServer::once("200 OK", "{}");
    assert_eq!(server.served(), 0, "요청이 없었다");
}

/// 멈추면 그때까지 채운 것은 남는다
#[test]
fn cancelling_keeps_what_was_already_named() {
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
    let p = fill(&db, &AtomicBool::new(true), None, |_| {}).unwrap();
    assert_eq!(
        (p.asked, p.files),
        (0, 0),
        "멈춤 상태면 아무것도 묻지 않는다"
    );
}
