use super::*;

/// 갈래 목록은 **지금 필터 안에서** 세야 한다. 전체를 세면 눌러도 0장인
/// 항목이 섞인다.
#[test]
fn facets_are_counted_inside_the_current_filter() {
    let (_d, db) = seeded();
    let all = facets(&db, &Filter::default(), FacetKind::Kind).unwrap();
    assert_eq!(all.iter().map(|f| f.count).sum::<i64>(), 50);

    // 영상만 걸어 두면 갈래도 영상만 남는다
    let only_video = Filter {
        kind: Some(1),
        ..Default::default()
    };
    let v = facets(&db, &only_video, FacetKind::Kind).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].label, "영상");
    assert_eq!(v[0].count, 5);
}

#[test]
fn facet_labels_are_readable() {
    let (_d, db) = seeded();
    let r = facets(&db, &Filter::default(), FacetKind::Rating).unwrap();
    assert!(r.iter().any(|f| f.label == "평점 없음"));
    assert!(r.iter().any(|f| f.label.starts_with('★')));

    let y = facets(&db, &Filter::default(), FacetKind::Year).unwrap();
    assert!(y.iter().all(|f| f.label.ends_with('년')), "{y:?}");
    // 연도는 최근이 위
    for w in y.windows(2) {
        assert!(w[0].value >= w[1].value);
    }
}

/// 태그는 폴더와 달리 한 장에 여럿 붙는다. 필터가 그중 하나만 걸려도
/// 그 사진이 나와야 한다.
#[test]
fn tag_filter_matches_any_of_a_files_tags() {
    let (_d, db) = seeded();
    db.transaction(|tx| {
        tx.execute("INSERT INTO tags(id,name) VALUES(1,'여행'),(2,'가족')", [])?;
        // 1~5번은 여행, 4~6번은 가족 — 4·5번은 둘 다
        for i in 1..=5 {
            tx.execute("INSERT INTO file_tags(file_id,tag_id) VALUES(?,1)", [i])?;
        }
        for i in 4..=6 {
            tx.execute("INSERT INTO file_tags(file_id,tag_id) VALUES(?,2)", [i])?;
        }
        Ok(())
    })
    .unwrap();

    let f = |t: i64| Filter {
        tag_id: Some(t),
        ..Default::default()
    };
    assert_eq!(summary(&db, &f(1)).unwrap().0, 5);
    assert_eq!(summary(&db, &f(2)).unwrap().0, 3);

    // 겹치는 두 장이 양쪽에 다 들어 있어야 한다
    let a: Vec<i64> = page(&db, &f(1), None, 99, GroupBy::None)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    let b: Vec<i64> = page(&db, &f(2), None, 99, GroupBy::None)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    assert!(a.contains(&4) && b.contains(&4));
    assert!(a.contains(&5) && b.contains(&5));
    // 없는 태그는 빈 목록
    assert_eq!(summary(&db, &f(99)).unwrap().0, 0);
}

/// 자리 갈래는 0.1도 격자다. 같은 칸에 든 것이 한 줄로 모여야 하고,
/// 그 값을 필터로 되돌려 걸면 같은 장수가 나와야 한다.
#[test]
fn place_facet_grids_coordinates_and_round_trips() {
    let (_d, db) = seeded();
    db.write(|c| {
        // 1~4번은 서울 한 칸(37.55, 126.98), 5번은 다른 칸
        c.execute(
            "UPDATE files SET gps_lat=37.55, gps_lon=126.98 WHERE id<=4",
            [],
        )?;
        c.execute(
            "UPDATE files SET gps_lat=35.15, gps_lon=129.05 WHERE id=5",
            [],
        )
    })
    .unwrap();

    let fs = facets(&db, &Filter::default(), FacetKind::Place).unwrap();
    // 좌표 없는 것들이 한 줄, 서울 한 줄, 부산 한 줄
    let none = fs.iter().find(|f| f.value.is_empty()).unwrap();
    assert_eq!(none.count, 45);
    assert_eq!(none.label, "(위치 정보 없음)");

    let seoul = fs.iter().find(|f| f.value.starts_with("37.5")).unwrap();
    assert_eq!(seoul.count, 4);
    assert_eq!(seoul.label, "북위 37.5° 동경 126.9°");

    // 갈래가 준 값을 그대로 필터로 되돌린다
    for f in &fs {
        let n = summary(
            &db,
            &Filter {
                place: Some(f.value.clone()),
                ..Default::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(n, f.count, "{} 되돌리기", f.label);
    }
}

/// 남반구·서반구 좌표도 같은 칸에서 갈라지면 안 된다 — 음수를 내림할 때
/// 0 쪽으로 자르면 -0.05와 0.05가 같은 칸에 들어간다.
#[test]
fn place_grid_handles_negative_coordinates() {
    let (_d, db) = seeded();
    db.write(|c| {
        c.execute(
            "UPDATE files SET gps_lat=-33.87, gps_lon=-70.65 WHERE id=1",
            [],
        )?;
        c.execute(
            "UPDATE files SET gps_lat=-33.83, gps_lon=-70.61 WHERE id=2",
            [],
        )?;
        c.execute("UPDATE files SET gps_lat=0.05, gps_lon=0.05 WHERE id=3", [])?;
        c.execute(
            "UPDATE files SET gps_lat=-0.05, gps_lon=-0.05 WHERE id=4",
            [],
        )
    })
    .unwrap();

    let fs = facets(&db, &Filter::default(), FacetKind::Place).unwrap();
    // -33.87과 -33.83은 다른 칸(-33.9 / -33.9? 아니다: -33.9와 -33.9)
    // 중요한 건 0을 사이에 둔 3·4번이 갈라지는 것이다
    let a = fs.iter().find(|f| f.value == "0.0,0.0").map(|f| f.count);
    let b = fs.iter().find(|f| f.value == "-0.1,-0.1").map(|f| f.count);
    assert_eq!(a, Some(1), "{fs:?}");
    assert_eq!(b, Some(1), "{fs:?}");

    let south = fs.iter().find(|f| f.value.starts_with("-33")).unwrap();
    assert!(south.label.starts_with("남위"), "{}", south.label);
    assert!(south.label.contains("서경"), "{}", south.label);

    // 음수에서도 갈래가 준 값이 그대로 필터로 되돌아가야 한다.
    // 격자 상자를 `[v, v+0.1)`로 잡는데 v가 음수면 부동소수 오차가
    // 반대쪽으로 새기 쉽다.
    for f in &fs {
        let n = summary(
            &db,
            &Filter {
                place: Some(f.value.clone()),
                ..Default::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(n, f.count, "{} 되돌리기", f.label);
    }
}

/// 위치 없음 센티널 (0, 0)은 갈래·필터·지도에서 모두 같은 뜻이어야 한다.
/// 예전 ROUND(x*10-.5)는 이를 -0.1 칸으로 보낸 뒤, 그 칸 필터에서는 0을
/// 제외해 «2천 장을 눌렀더니 0장»이 됐다.
#[test]
fn zero_zero_is_missing_and_exact_grid_boundaries_round_trip() {
    let (_d, db) = seeded();
    db.write(|c| {
        c.execute(
            "UPDATE files SET gps_lat=0.0, gps_lon=0.0 WHERE id IN (1,2)",
            [],
        )?;
        c.execute("UPDATE files SET gps_lat=0.05, gps_lon=0.05 WHERE id=3", [])
    })
    .unwrap();

    let fs = facets(&db, &Filter::default(), FacetKind::Place).unwrap();
    let none = fs.iter().find(|f| f.value.is_empty()).unwrap();
    assert_eq!(none.count, 49, "NULL 47장과 (0,0) 두 장");
    assert!(fs.iter().all(|f| f.value != "-0.1,-0.1"), "{fs:?}");

    let origin_cell = fs.iter().find(|f| f.value == "0.0,0.0").unwrap();
    assert_eq!(origin_cell.count, 1);
    let round_trip = summary(
        &db,
        &Filter {
            place: Some(origin_cell.value.clone()),
            ..Default::default()
        },
    )
    .unwrap()
    .0;
    assert_eq!(round_trip, 1);
    assert_eq!(
        summary(
            &db,
            &Filter {
                place: Some(String::new()),
                ..Default::default()
            }
        )
        .unwrap()
        .0,
        none.count
    );
}

/// 상태바의 «썸네일 없음 N장»과 그걸 눌렀을 때 뜨는 장수가 같아야 한다
#[test]
fn bbox_parses_four_numbers_in_order() {
    assert_eq!(
        parse_bbox("37.4,126.8,37.6,127.1"),
        Some([37.4, 126.8, 37.6, 127.1])
    );
    assert_eq!(parse_bbox("37.6,126.8,37.4,127.1"), None); // 남이 북보다 크다
    assert_eq!(parse_bbox("1,2,3"), None);
    assert_eq!(parse_bbox("0,x,1,2,3"), None);
    assert_eq!(parse_bbox("NaN,0,1,1"), None);
    assert_eq!(parse_bbox("-91,0,1,1"), None);
    assert_eq!(parse_bbox("0,-181,1,1"), None);
}

#[test]
fn map_cells_group_by_grid_and_respect_the_bbox() {
    let (_d, db) = seeded();
    db.transaction(|tx| {
        // 서울 둘, 부산 하나
        tx.execute(
            "UPDATE files SET gps_lat = 37.55, gps_lon = 126.98 WHERE id IN (1, 2)",
            [],
        )?;
        tx.execute(
            "UPDATE files SET gps_lat = 35.18, gps_lon = 129.08 WHERE id = 3",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let cells = map_cells(&db, &Filter::default(), 1.0).unwrap();
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].n, 2);
    assert!((cells[0].lat - 37.55).abs() < 1e-6);
    let seoul = Filter {
        bbox: Some("37,126,38,128".into()),
        ..Default::default()
    };
    assert_eq!(summary(&db, &seoul).unwrap().0, 2);
    assert_eq!(map_cells(&db, &seoul, 0.1).unwrap().len(), 1);
}

/// 지도에서 위경도만 보고는 어디인지 알 수 없다 — 칸마다 지명을 함께 준다.
///
/// 대표 지명은 **가장 흔한 것**이어야 한다. 사전순 첫째를 쓰면 한 장짜리
/// 옆 동네가 그 자리를 대표하게 된다.
#[test]
fn a_map_cell_carries_the_place_name_people_would_call_it() {
    let (_d, db) = seeded();
    db.transaction(|tx| {
        // 한 칸 안에 수원 셋, 용인 하나 — 대표는 수원이어야 한다
        tx.execute(
            "UPDATE files SET gps_lat=37.28, gps_lon=127.01, geo_name='수원시' WHERE id IN (1,2,3)",
            [],
        )?;
        tx.execute(
            "UPDATE files SET gps_lat=37.24, gps_lon=127.17, geo_name='용인시' WHERE id=4",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let wide = map_cells(&db, &Filter::default(), 1.0).unwrap();
    assert_eq!(wide.len(), 1, "1도 칸이면 넷이 한 자리에 모인다");
    assert_eq!(
        wide[0].place.as_deref(),
        Some("수원시"),
        "한 장짜리 옆 동네가 이기면 안 된다"
    );
    assert_eq!(wide[0].places, 2, "섞인 곳이 둘이라고 알려 준다");

    // 줌을 당기면 각자 제 이름을 가진다
    let close = map_cells(&db, &Filter::default(), 0.1).unwrap();
    assert_eq!(close.len(), 2);
    let mut names: Vec<_> = close.iter().map(|c| (c.place.clone(), c.places)).collect();
    names.sort();
    assert_eq!(
        names,
        vec![(Some("수원시".into()), 1), (Some("용인시".into()), 1)]
    );
}

/// 칸 크기가 숫자가 아니면 지도가 통째로 비었다 — 기본 칸으로 돌린다
#[test]
fn a_nonsense_cell_size_falls_back_instead_of_breaking_the_map() {
    let (_d, db) = seeded();
    db.transaction(|tx| {
        tx.execute(
            "UPDATE files SET gps_lat=37.28, gps_lon=127.01 WHERE id IN (1,2)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let cells = map_cells(&db, &Filter::default(), bad).unwrap();
        assert_eq!(cells.len(), 1, "{bad} 에서 지도가 비었습니다");
        assert_eq!(cells[0].n, 2);
    }
}

/// 아직 지명을 안 채웠으면 이름 자리는 비어 있고, 장수는 그대로 보인다
#[test]
fn a_map_cell_without_a_name_still_counts_its_photos() {
    let (_d, db) = seeded();
    db.transaction(|tx| {
        tx.execute(
            "UPDATE files SET gps_lat=37.28, gps_lon=127.01 WHERE id IN (1,2)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let cells = map_cells(&db, &Filter::default(), 1.0).unwrap();
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].n, 2);
    assert_eq!(cells[0].place, None);
    assert_eq!(
        cells[0].places, 0,
        "이름이 없는 것은 «한 곳»이 아니라 «없음»이다"
    );
}

#[test]
fn map_overview_excludes_missing_sentinels_and_keeps_global_bounds() {
    let (_d, db) = seeded();
    db.transaction(|tx| {
        tx.execute(
            "UPDATE files SET gps_lat=0.0, gps_lon=0.0 WHERE id IN (1,2)",
            [],
        )?;
        tx.execute(
            "UPDATE files SET gps_lat=37.55, gps_lon=126.98 WHERE id=3",
            [],
        )?;
        tx.execute(
            "UPDATE files SET gps_lat=35.18, gps_lon=129.08 WHERE id=4",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let o = map_overview(&db, &Filter::default()).unwrap();
    assert_eq!(o.total, 2);
    assert_eq!(o.bounds, Some([35.18, 126.98, 37.55, 129.08]));
    assert_eq!(map_cells(&db, &Filter::default(), 0.1).unwrap().len(), 2);
}

#[test]
fn no_thumb_filter_matches_the_pending_count() {
    let (_d, db) = seeded();
    db.write(|c| {
        c.execute(
            "INSERT INTO thumbs(file_id,rel_path,src_size,src_mtime,state)
                 SELECT id,'x',1,1,1 FROM files WHERE id <= 40",
            [],
        )?;
        // 하나는 실패한 것
        c.execute(
            "INSERT INTO thumbs(file_id,rel_path,src_size,src_mtime,state) VALUES(41,NULL,1,1,2)",
            [],
        )
    })
    .unwrap();
    let n = summary(
        &db,
        &Filter {
            no_thumb: true,
            ..Default::default()
        },
    )
    .unwrap()
    .0;
    assert_eq!(n, 10, "40장은 됐고 41은 실패, 42~50은 아직 — 열 장");
}

#[test]
fn day_facet_and_filter_round_trip() {
    let (_d, db) = seeded();
    // 한 달 안에서 날짜별로 센다 — 갈래 값을 필터로 되돌리면 같은 수
    let month = facets(&db, &Filter::default(), FacetKind::Year).unwrap();
    assert!(!month.is_empty());
    let fs = facets(&db, &Filter::default(), FacetKind::Day).unwrap();
    assert!(fs.iter().all(|f| f.label.ends_with('일')), "{fs:?}");
    for f in &fs {
        let n = summary(
            &db,
            &Filter {
                day: Some(f.value.clone()),
                ..Default::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(n, f.count, "{} 되돌리기", f.value);
    }
    // 최근이 위
    for w in fs.windows(2) {
        assert!(w[0].value >= w[1].value);
    }
}

#[test]
fn lens_facet_and_filter_round_trip() {
    let (_d, db) = seeded();
    db.write(|c| c.execute("UPDATE files SET lens='FE 24-70' WHERE id <= 3", []))
        .unwrap();
    let fs = facets(&db, &Filter::default(), FacetKind::Lens).unwrap();
    assert!(fs.iter().any(|f| f.label == "(렌즈 정보 없음)"), "{fs:?}");
    for f in &fs {
        let n = summary(
            &db,
            &Filter {
                lens: Some(f.value.clone()),
                ..Default::default()
            },
        )
        .unwrap()
        .0;
        assert_eq!(n, f.count, "{} 되돌리기", f.label);
    }
}

#[test]
fn camera_facet_names_the_unknown() {
    let (_d, db) = seeded();
    let c = facets(&db, &Filter::default(), FacetKind::Camera).unwrap();
    assert!(c.iter().any(|f| f.label == "(카메라 정보 없음)"), "{c:?}");
}

#[test]
fn summary_reports_count_and_bytes() {
    let (_d, db) = seeded();
    let (n, bytes) = summary(&db, &Filter::default()).unwrap();
    assert_eq!(n, 50);
    assert_eq!(bytes, (1..=50).map(|i| i * 100).sum::<i64>());
}
