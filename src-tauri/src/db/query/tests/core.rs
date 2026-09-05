use super::*;

#[test]
fn cursor_at_lands_on_the_same_row_as_a_full_read() {
    let (_d, db) = seeded();
    let f = Filter::default();
    let all = page(&db, &f, None, 500, GroupBy::None).unwrap().rows;
    assert_eq!(all.len(), 50);

    for index in [0usize, 1, 7, 23, 49] {
        let c = cursor_at(&db, &f, index as i64).unwrap();
        let got = page(&db, &f, c, 3, GroupBy::None).unwrap().rows;
        assert_eq!(got[0].id, all[index].id, "{index}번째에서 시작해야 한다");
    }
}

#[test]
fn cursor_at_respects_the_filter() {
    let (_d, db) = seeded();
    // 영상만 — 10개마다 하나라 5장이다
    let f = Filter {
        kind: Some(1),
        ..Default::default()
    };
    let all = page(&db, &f, None, 500, GroupBy::None).unwrap().rows;
    assert_eq!(all.len(), 5);
    let c = cursor_at(&db, &f, 3).unwrap();
    let got = page(&db, &f, c, 5, GroupBy::None).unwrap().rows;
    assert_eq!(got[0].id, all[3].id);
    assert_eq!(got.len(), 2, "3번째부터 끝까지");
}

/// area 필터는 folders를 봐야 해서 조인을 살린다. 그 분기도 맞아야 한다.
#[test]
fn cursor_at_keeps_the_join_for_area() {
    let (_d, db) = seeded();
    let f = Filter {
        area: Some(2),
        ..Default::default()
    };
    let all = page(&db, &f, None, 500, GroupBy::None).unwrap().rows;
    assert_eq!(all.len(), 10, "폴더 3(area=2)에 41~50번 10장");
    let c = cursor_at(&db, &f, 4).unwrap();
    let got = page(&db, &f, c, 10, GroupBy::None).unwrap().rows;
    assert_eq!(got[0].id, all[4].id);
    assert_eq!(got.len(), 6);
}

#[test]
fn cursor_at_edges() {
    let (_d, db) = seeded();
    let f = Filter::default();
    assert!(
        cursor_at(&db, &f, 0).unwrap().is_none(),
        "맨 앞은 커서가 없다"
    );
    assert!(
        cursor_at(&db, &f, -5).unwrap().is_none(),
        "음수도 맨 앞으로"
    );
    // 끝을 넘어가면 행이 없다 — 빈 페이지가 되지 손잡이가 깨지면 안 된다
    assert!(cursor_at(&db, &f, 9999).unwrap().is_none());
}

/// 경로 앞부분으로 폴더와 그 아래를 고른다. 사이드바 트리의 중간 마디는
/// DB 행이 없어 id로는 못 고른다.
/// 버린 사진이 목록에 남아 있으면 원본은 없는데 타일만 뜬다.
#[test]
fn trashed_files_disappear_from_the_default_view() {
    let (_d, db) = seeded();
    let all = page(&db, &Filter::default(), None, 500, GroupBy::None)
        .unwrap()
        .rows
        .len();
    db.write(|c| c.execute("UPDATE files SET trashed_at=1 WHERE id IN (1,2,3)", []))
        .unwrap();

    assert_eq!(
        page(&db, &Filter::default(), None, 500, GroupBy::None)
            .unwrap()
            .rows
            .len(),
        all - 3
    );
    assert_eq!(summary(&db, &Filter::default()).unwrap().0, all as i64 - 3);
    assert_eq!(
        timeline(&db, &Filter::default())
            .unwrap()
            .iter()
            .map(|b| b.count)
            .sum::<i64>(),
        all as i64 - 3
    );

    // 휴지통 보기에서는 그것만 나온다
    let t = Filter {
        trashed: true,
        ..Default::default()
    };
    assert_eq!(
        page(&db, &t, None, 500, GroupBy::None).unwrap().rows.len(),
        3
    );
}

/// 정렬 기준을 바꿔도 페이지가 끊기거나 겹치지 않아야 한다.
/// 커서 방향이 정렬 방향과 어긋나면 딱 그 증상이 난다.
#[test]
fn every_sort_pages_without_gaps_or_overlaps() {
    let (_d, db) = seeded();
    for by in [
        SortBy::TakenAt,
        SortBy::CreatedAt,
        SortBy::ModifiedAt,
        SortBy::Name,
        SortBy::Size,
        SortBy::Pixels,
        SortBy::Duration,
    ] {
        for desc in [true, false] {
            let f = Filter {
                sort: Sort { by, desc },
                ..Default::default()
            };
            // 한 번에 다 읽은 것과 7장씩 넘겨 읽은 것이 같아야 한다
            let all: Vec<i64> = page(&db, &f, None, 500, GroupBy::None)
                .unwrap()
                .rows
                .iter()
                .map(|r| r.id)
                .collect();
            let mut paged = Vec::new();
            let mut cur = None;
            loop {
                let p = page(&db, &f, cur, 7, GroupBy::None).unwrap();
                paged.extend(p.rows.iter().map(|r| r.id));
                match p.next {
                    Some(c) => cur = Some(c),
                    None => break,
                }
            }
            assert_eq!(all, paged, "{by:?} desc={desc}");
            assert_eq!(all.len(), 50, "{by:?} desc={desc} — 빠진 것이 없어야 한다");
        }
    }
}

/// 그룹 값은 **행에 붙어** 온다. 이어 읽은 페이지의 첫 줄이 앞 페이지
/// 마지막과 같은 그룹이면 머리글을 또 넣으면 안 되는데, 값이 붙어 있으면
/// 그 비교가 저절로 된다.
#[test]
fn group_values_ride_along_with_each_row() {
    let (_d, db) = seeded();
    let f = Filter::default();

    let none = page(&db, &f, None, 5, GroupBy::None).unwrap().rows;
    assert!(
        none.iter().all(|r| r.group.is_none()),
        "안 묶으면 비어 있다"
    );

    for g in [
        GroupBy::Folder,
        GroupBy::Day,
        GroupBy::Month,
        GroupBy::Year,
        GroupBy::Rating,
        GroupBy::FileType,
        GroupBy::Culling,
        GroupBy::Camera,
        GroupBy::Lens,
    ] {
        let rows = page(&db, &f, None, 50, g).unwrap().rows;
        assert!(
            rows.iter().all(|r| r.group.is_some()),
            "{g:?} — 모든 행에 값이 있어야 한다"
        );
    }
}

/// 페이지를 넘어가도 그룹 값이 이어져야 한다.
#[test]
fn group_values_survive_paging() {
    let (_d, db) = seeded();
    let f = Filter::default();
    let all: Vec<Option<String>> = page(&db, &f, None, 500, GroupBy::Day)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.group.clone())
        .collect();

    let mut paged = Vec::new();
    let mut cur = None;
    loop {
        let p = page(&db, &f, cur, 6, GroupBy::Day).unwrap();
        paged.extend(p.rows.iter().map(|r| r.group.clone()));
        match p.next {
            Some(c) => cur = Some(c),
            None => break,
        }
    }
    assert_eq!(all, paged);
}

#[test]
fn file_type_group_is_readable() {
    let (_d, db) = seeded();
    let rows = page(&db, &Filter::default(), None, 50, GroupBy::FileType)
        .unwrap()
        .rows;
    let names: std::collections::HashSet<String> =
        rows.iter().filter_map(|r| r.group.clone()).collect();
    assert!(names.contains("사진"), "{names:?}");
    assert!(names.contains("영상"), "{names:?}");
}

#[test]
fn ascending_and_descending_are_mirror_images() {
    let (_d, db) = seeded();
    let asc = Filter {
        sort: Sort {
            by: SortBy::Size,
            desc: false,
        },
        ..Default::default()
    };
    let desc = Filter {
        sort: Sort {
            by: SortBy::Size,
            desc: true,
        },
        ..Default::default()
    };
    let a: Vec<i64> = page(&db, &asc, None, 500, GroupBy::None)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    let mut d: Vec<i64> = page(&db, &desc, None, 500, GroupBy::None)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    d.reverse();
    assert_eq!(a, d);
}

/// 스크롤바가 준 순번은 **지금 정렬 기준**의 순번이어야 한다.
#[test]
fn cursor_at_follows_the_current_sort() {
    let (_d, db) = seeded();
    let f = Filter {
        sort: Sort {
            by: SortBy::Name,
            desc: false,
        },
        ..Default::default()
    };
    let all = page(&db, &f, None, 500, GroupBy::None).unwrap().rows;
    for i in [0usize, 5, 30, 49] {
        let c = cursor_at(&db, &f, i as i64).unwrap();
        let got = page(&db, &f, c, 3, GroupBy::None).unwrap().rows;
        assert_eq!(got[0].id, all[i].id, "{i}번째");
    }
}

#[test]
fn folder_path_selects_the_subtree() {
    let (_d, db) = seeded();
    // 폴더 1 = 'a', 폴더 2 = 'a/b', 폴더 3 = 'z'
    let f = Filter {
        folder_path: Some("a".into()),
        ..Default::default()
    };
    let n = page(&db, &f, None, 500, GroupBy::None).unwrap().rows.len();
    assert_eq!(n, 40, "a(30) + a/b(10)");

    let only_b = Filter {
        folder_path: Some("a/b".into()),
        ..Default::default()
    };
    assert_eq!(
        page(&db, &only_b, None, 500, GroupBy::None)
            .unwrap()
            .rows
            .len(),
        10
    );

    // 이름이 겹치는 형제를 잡아먹으면 안 된다
    let none = Filter {
        folder_path: Some("a/bb".into()),
        ..Default::default()
    };
    assert_eq!(
        page(&db, &none, None, 500, GroupBy::None)
            .unwrap()
            .rows
            .len(),
        0
    );
}

/// LIKE의 `_`는 아무 글자나 매치한다. 실제 라이브러리에 `#0_사진백업…`
/// 같은 폴더가 있어 이스케이프하지 않으면 엉뚱한 폴더까지 딸려온다.
#[test]
fn folder_path_escapes_like_wildcards() {
    let (dir, db) = seeded();
    let _ = dir;
    db.write(|c| {
        c.execute(
            "INSERT INTO folders(id,volume_uuid,rel_path,name,area) VALUES
                   (10,'V','p_q','p_q',1),(11,'V','pXq','pXq',1)",
            [],
        )?;
        c.execute(
            "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at)
                 VALUES(101,10,'a.jpg',1,0,1000,0,0),(102,11,'b.jpg',1,0,1000,0,0)",
            [],
        )
    })
    .unwrap();

    let f = Filter {
        folder_path: Some("p_q".into()),
        ..Default::default()
    };
    let rows = page(&db, &f, None, 500, GroupBy::None).unwrap().rows;
    assert_eq!(rows.len(), 1, "pXq까지 잡히면 안 된다");
    assert_eq!(rows[0].name, "a.jpg");
}

#[test]
fn partial_filter_json_deserializes() {
    // 프론트는 { folder_id: 1 }처럼 일부만 보낸다.
    // serde(default)가 없으면 "missing field favorite_only"로 커맨드가 거부된다.
    let f: Filter = serde_json::from_str(r#"{"folder_id":1}"#).expect("일부 필드만");
    assert_eq!(f.folder_id, Some(1));
    assert!(!f.favorite_only);
    let empty: Filter = serde_json::from_str("{}").expect("빈 객체");
    assert!(empty.folder_id.is_none());
    // null도 받아들여야 한다
    let nulls: Filter = serde_json::from_str(r#"{"folder_id":null,"kind":null}"#).expect("null");
    assert!(nulls.folder_id.is_none());
}

#[test]
fn first_page_is_newest_first() {
    let (_d, db) = seeded();
    let p = page(&db, &Filter::default(), None, 10, GroupBy::None).unwrap();
    assert_eq!(p.rows.len(), 10);
    assert!(p.next.is_some());
    // 내림차순인지
    for w in p.rows.windows(2) {
        assert!(
            (w[0].taken_at, w[0].id) > (w[1].taken_at, w[1].id),
            "최신순이어야 한다"
        );
    }
}

#[test]
fn paging_covers_everything_exactly_once() {
    let (_d, db) = seeded();
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let p = page(&db, &Filter::default(), cursor, 7, GroupBy::None).unwrap();
        seen.extend(p.rows.iter().map(|r| r.id));
        match p.next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen.len(), 50, "빠짐없이");
    let mut uniq = seen.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 50, "겹침 없이");
}

#[test]
fn folder_filter_includes_subfolders() {
    let (_d, db) = seeded();
    // 폴더 1(a)에는 30장, 하위 폴더 2(a/b)에 10장 → 40장
    let f = Filter {
        folder_id: Some(1),
        ..Default::default()
    };
    let (n, _) = summary(&db, &f).unwrap();
    assert_eq!(n, 40, "하위 폴더를 포함해야 한다");
}

#[test]
fn area_filter_separates_regions() {
    let (_d, db) = seeded();
    let mine = Filter {
        area: Some(1),
        ..Default::default()
    };
    let shared = Filter {
        area: Some(2),
        ..Default::default()
    };
    assert_eq!(summary(&db, &mine).unwrap().0, 40);
    assert_eq!(summary(&db, &shared).unwrap().0, 10);
}

#[test]
fn rating_and_culling_filters() {
    let (_d, db) = seeded();
    let high = Filter {
        min_rating: Some(4),
        ..Default::default()
    };
    let (n, _) = summary(&db, &high).unwrap();
    assert!(n > 0 && n < 50);
    for r in page(&db, &high, None, 100, GroupBy::None).unwrap().rows {
        assert!(r.rating >= 4);
    }
    let rejected = Filter {
        culling_flag: Some(2),
        ..Default::default()
    };
    for r in page(&db, &rejected, None, 100, GroupBy::None).unwrap().rows {
        assert_eq!(r.culling_flag, 2);
    }
}

#[test]
fn name_search_escapes_wildcards() {
    let (_d, db) = seeded();
    // "IMG_0001"의 밑줄이 와일드카드로 동작하면 안 된다
    let f = Filter {
        name_like: Some("IMG_0001".into()),
        ..Default::default()
    };
    let p = page(&db, &f, None, 100, GroupBy::None).unwrap();
    assert_eq!(p.rows.len(), 1, "정확히 하나만");
    assert_eq!(p.rows[0].name, "IMG_0001.jpg");

    // 밑줄이 와일드카드였다면 "IMGX0001"도 걸렸을 것이다
    let f2 = Filter {
        name_like: Some("IMG".into()),
        ..Default::default()
    };
    assert_eq!(
        page(&db, &f2, None, 100, GroupBy::None).unwrap().rows.len(),
        50
    );
}

#[test]
fn thumb_is_none_until_generated() {
    let (_d, db) = seeded();
    let p = page(&db, &Filter::default(), None, 5, GroupBy::None).unwrap();
    assert!(p.rows.iter().all(|r| r.thumb.is_none()));

    db.write(|c| {
        c.execute(
            "INSERT INTO thumbs(file_id,rel_path,src_size,src_mtime,state)
                 VALUES(50,'ab/abcd.jpg',1,1,1)",
            [],
        )
    })
    .unwrap();
    let p2 = page(&db, &Filter::default(), None, 5, GroupBy::None).unwrap();
    assert_eq!(
        p2.rows.iter().filter(|r| r.thumb.is_some()).count(),
        1,
        "만들어진 것만 경로가 있다"
    );
}

#[test]
fn failed_thumbs_are_not_served() {
    let (_d, db) = seeded();
    db.write(|c| {
        c.execute(
            "INSERT INTO thumbs(file_id,rel_path,src_size,src_mtime,state)
                 VALUES(50,'ab/x.jpg',1,1,2)", // state 2 = 실패
            [],
        )
    })
    .unwrap();
    let p = page(&db, &Filter::default(), None, 5, GroupBy::None).unwrap();
    assert!(
        p.rows.iter().all(|r| r.thumb.is_none()),
        "실패한 썸네일은 내보내지 않는다"
    );
}

#[test]
fn timeline_groups_by_month_newest_first() {
    let (_d, db) = seeded();
    let b = timeline(&db, &Filter::default()).unwrap();
    assert!(!b.is_empty());
    // 내림차순
    for w in b.windows(2) {
        assert!((w[0].year, w[0].month) >= (w[1].year, w[1].month));
    }
    // 합계가 전체와 같아야 한다
    assert_eq!(b.iter().map(|x| x.count).sum::<i64>(), 50);
    // top으로 그 지점부터 읽을 수 있어야 한다
    let first = &b[0];
    let p = page(
        &db,
        &Filter::default(),
        Some(Cursor {
            num: Some(first.top + 1),
            text: None,
            id: i64::MAX,
        }),
        5,
        GroupBy::None,
    )
    .unwrap();
    assert!(!p.rows.is_empty(), "점프 지점부터 읽힌다");
}

/// `strftime('%Y-%m')` 한 번으로 바꾸면서 Rust가 문자열을 쪼갠다.
/// 연·월이 정수로 제대로 나오는지, 필터가 걸려도 그런지 본다.
#[test]
fn timeline_parses_year_and_month_as_numbers() {
    let (_d, db) = seeded();
    for f in [
        Filter::default(),
        Filter {
            area: Some(1),
            ..Default::default()
        },
    ] {
        let b = timeline(&db, &f).unwrap();
        assert!(!b.is_empty());
        for x in &b {
            assert!(x.year >= 1970 && x.year <= 2100, "연도: {}", x.year);
            assert!((1..=12).contains(&x.month), "월: {}", x.month);
            assert!(x.count > 0);
        }
    }
}

/// 조인을 빼는 최적화가 결과를 바꾸면 안 된다.
#[test]
fn dropping_the_join_keeps_the_same_totals() {
    let (_d, db) = seeded();
    // folders를 보는 필터(area)와 안 보는 필터(kind)가 서로 어긋나지 않아야 한다
    let all = timeline(&db, &Filter::default()).unwrap();
    assert_eq!(
        all.iter().map(|x| x.count).sum::<i64>(),
        summary(&db, &Filter::default()).unwrap().0
    );
    let area = Filter {
        area: Some(2),
        ..Default::default()
    };
    assert_eq!(
        timeline(&db, &area)
            .unwrap()
            .iter()
            .map(|x| x.count)
            .sum::<i64>(),
        summary(&db, &area).unwrap().0
    );
}
