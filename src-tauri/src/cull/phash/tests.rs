use super::*;

/// 32×32 회색조 — 사진관 쪽 시험과 **같은 식**으로 만든다.
fn pixels() -> Vec<u8> {
    (0..32)
        .flat_map(|y| (0..32).map(move |x| (((x * 7 + y * 13 + (x * y) % 11) * 3) % 256) as u8))
        .collect()
}

/// 우리집 사진관이 같은 화소에 내놓는 값. 리사이즈는 라이브러리마다 다르므로
/// **줄이기를 뺀 나머지**(DCT·중앙값·비트)가 같은지를 못 박는다. 이 값이 어긋나면
/// 두 앱이 같은 사진을 다르게 보게 된다.
///
/// 얻은 법: photo_gallery/backend 에서
/// `Image.frombytes("L",(32,32),data)` → `app.photos.hashing.phash_hex`.
#[test]
fn matches_the_gallery_on_the_same_pixels() {
    assert_eq!(phash_of_gray(&pixels()), 0xad42_4c63_bd93_9d23);
}

fn save(path: &Path, w: u32, h: u32) {
    // 결이 있는 그림 — 밋밋하면 해시가 다 같아져 시험이 뜻을 잃는다
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        let v = ((fx * 6.0).sin() * 90.0 + (fy * 9.0).cos() * 70.0 + 128.0) as u8;
        let s = if (x * 5 / w) % 2 == (y * 5 / h) % 2 {
            40
        } else {
            0
        };
        image::Rgb([v.saturating_add(s), v, v.saturating_sub(s)])
    });
    img.save(path).unwrap();
}

#[test]
fn a_shrunk_copy_stays_within_the_threshold() {
    let d = tempfile::tempdir().unwrap();
    let (big, small) = (d.path().join("big.png"), d.path().join("small.png"));
    save(&big, 800, 600);
    save(&small, 200, 150); // 같은 그림을 1/4 로
    let (a, sa) = signature_of(&big).unwrap();
    let (b, sb) = signature_of(&small).unwrap();
    assert!(
        hamming(a, b) <= DEFAULT_THRESHOLD,
        "줄인 사본인데 {}비트나 달랐다 ({a:016x} vs {b:016x})",
        hamming(a, b)
    );
    assert!(
        signatures_alike(&sa, &sb),
        "줄인 사본의 색차 안전판이 너무 좁다"
    );
}

#[test]
fn equal_luminance_but_different_colours_are_not_the_same_picture() {
    let luma = |rgb: image::Rgb<u8>| {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, rgb))
            .to_luma8()
            .get_pixel(0, 0)[0]
    };
    let red = image::Rgb([255, 0, 0]);
    let target = luma(red);
    // image 크레이트가 정확히 같은 밝기로 바꾸는 초록색을 고른다. 밝기 서명만
    // 있었다면 두 평면은 MAD 0으로 반드시 통과했다.
    let green = (0..=255)
        .map(|g| image::Rgb([0, g, 0]))
        .find(|&rgb| luma(rgb) == target)
        .expect("붉은색과 같은 밝기의 초록색");
    assert_ne!(red, green);

    let d = tempfile::tempdir().unwrap();
    let (a, b) = (d.path().join("red.png"), d.path().join("green.png"));
    image::RgbImage::from_pixel(64, 64, red).save(&a).unwrap();
    image::RgbImage::from_pixel(64, 64, green).save(&b).unwrap();
    let (_, sa) = signature_of(&a).unwrap();
    let (_, sb) = signature_of(&b).unwrap();
    assert_eq!(mad(&sa[1..1 + LUMA_BYTES], &sb[1..1 + LUMA_BYTES]), 0.0);
    assert!(
        !signatures_alike(&sa, &sb),
        "색차가 큰 편집본을 같은 사진으로 봤다"
    );
}

#[test]
fn a_different_picture_is_far_away() {
    let d = tempfile::tempdir().unwrap();
    let (a_p, b_p) = (d.path().join("a.png"), d.path().join("b.png"));
    save(&a_p, 400, 300);
    image::RgbImage::from_fn(400, 300, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 30])
    })
    .save(&b_p)
    .unwrap();
    let (a, b) = (phash_of(&a_p).unwrap(), phash_of(&b_p).unwrap());
    assert!(
        hamming(a, b) > DEFAULT_THRESHOLD,
        "다른 그림인데 {}비트만 달랐다",
        hamming(a, b)
    );
}

#[test]
fn an_unreadable_file_is_skipped_not_fatal() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("nope.png");
    std::fs::write(&p, b"not an image").unwrap();
    assert_eq!(phash_of(&p), None);
}

// ── DB ────────────────────────────────────────────────────────────────

fn db() -> (tempfile::TempDir, Db) {
    let d = tempfile::tempdir().unwrap();
    let db = Db::open(d.path().join("t.db")).unwrap();
    (d, db)
}

/// (id, phash, size, width, height, area, pic)
/// `pic` 는 **어느 그림인가** — 같으면 화소 서명이 같아 MAD 0, 다르면 40 이상 벌어진다.
/// 해시가 이어도 그림이 다르면 안 묶인다는 것을 시험이 말할 수 있게 하려고 둔다.
type SeedItem = (i64, u64, i64, i64, i64, i32, u8);

/// 그림 하나를 나타내는 서명 — 밝기만 다른 평면. 다른 `pic` 끼리는 MAD 가
/// 40 이상 벌어져 문턱(3.5)을 훌쩍 넘는다.
fn flat_sig(value: u8) -> Vec<u8> {
    let mut sig = vec![value; SIG_BYTES];
    sig[0] = SIGNATURE_VERSION;
    sig
}

fn sig_of(pic: u8) -> Vec<u8> {
    flat_sig(pic.saturating_mul(40))
}

fn seed(db: &Db, items: &[SeedItem]) {
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library')",
            [],
        )?;
        for area in [0, 1, 2] {
            tx.execute(
                "INSERT INTO folders(id,volume_uuid,rel_path,name,area)
                     VALUES(?1,'V',?2,?2,?1)",
                rusqlite::params![area, format!("f{area}")],
            )?;
        }
        for (id, hash, size, w, h, area, pic) in items {
            tx.execute(
                "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,
                        scanned_at,width,height,phash,psig)
                     VALUES(?1,?2,?3,?4,0,1000,0,0,?5,?6,?7,?8)",
                rusqlite::params![
                    id,
                    area,
                    format!("f{id}.jpg"),
                    size,
                    w,
                    h,
                    *hash as i64,
                    sig_of(*pic)
                ],
            )?;
        }
        Ok(())
    })
    .unwrap();
}

fn run(db: &Db) -> PhashProgress {
    let d = tempfile::tempdir().unwrap();
    scan(
        db,
        d.path(),
        DEFAULT_THRESHOLD,
        Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
}

fn members_of(db: &Db) -> Vec<(i64, bool)> {
    db.read(|c| {
        let mut st = c.prepare(
            "SELECT m.file_id, m.is_best FROM group_members m
                 JOIN groups g ON g.id = m.group_id WHERE g.kind = 4 ORDER BY m.file_id",
        )?;
        let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.collect()
    })
    .unwrap()
}

/// 한 비트 다른 해시 — 다시 인코딩하면 이만큼 흔들린다
const H: u64 = 0x0f0f_0f0f_0f0f_0f0f;

#[test]
fn groups_a_shrunk_copy_and_keeps_the_bigger_one() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 500, 400, 300, 0, 1),        // 줄인 사본
            (2, H ^ 1, 4000, 1600, 1200, 0, 1), // 원본
        ],
    );
    let p = run(&db);
    assert_eq!((p.groups, p.members), (1, 2));
    assert_eq!(
        members_of(&db),
        vec![(1, false), (2, true)],
        "큰 것이 대표여야 한다"
    );
    assert_eq!(p.reclaimable, 500);
}

#[test]
fn does_not_group_pictures_that_are_far_apart() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, 0, 100, 400, 300, 0, 1),
            (2, u64::MAX, 100, 400, 300, 0, 2),
        ],
    );
    assert_eq!(run(&db).groups, 0);
}

/// 가로세로 비가 다르면 «줄인 사본»이 아니라 잘라 낸 사진이다
#[test]
fn splits_a_crop_with_a_different_aspect_ratio() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 500, 400, 300, 0, 1),        // 4:3
            (2, H ^ 1, 4000, 1600, 1200, 0, 1), // 4:3 — 1과 한 무리
            (3, H ^ 2, 900, 400, 400, 0, 1),    // 1:1 — 갈라져 나가 혼자 남는다
        ],
    );
    let p = run(&db);
    assert_eq!((p.groups, p.members), (1, 2));
    assert_eq!(
        members_of(&db).iter().map(|m| m.0).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

/// 완전 중복이 이미 «뺄 것»으로 표시한 사본은 여기서 또 보여 주지 않는다
#[test]
fn leaves_out_copies_the_exact_duplicate_pass_already_took() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 500, 400, 300, 0, 1),
            (2, H, 500, 400, 300, 0, 1),
            (3, H ^ 1, 4000, 1600, 1200, 0, 1),
        ],
    );
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO groups(id,kind,reason,size_bytes,state,created_at)
                 VALUES(9,0,'같음',500,0,0)",
            [],
        )?;
        // 1이 대표, 2는 뺄 것 — 2는 이 갈래에서 빠져야 한다
        tx.execute(
            "INSERT INTO group_members(group_id,file_id,is_best) VALUES(9,1,1)",
            [],
        )?;
        tx.execute(
            "INSERT INTO group_members(group_id,file_id,is_best) VALUES(9,2,0)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let p = run(&db);
    assert_eq!((p.groups, p.members), (1, 2));
    assert_eq!(
        members_of(&db).iter().map(|m| m.0).collect::<Vec<_>>(),
        vec![1, 3]
    );
}

/// 화소 수가 같으면 정리된 자리(내사진·공용)에 있는 것이 대표가 된다
#[test]
fn prefers_the_settled_copy_when_the_size_is_the_same() {
    let (_d, db) = db();
    seed(
        &db,
        &[(1, H, 100, 400, 300, 0, 1), (2, H, 100, 400, 300, 2, 1)],
    );
    run(&db);
    assert_eq!(members_of(&db), vec![(1, false), (2, true)]);
}

#[test]
fn rerunning_replaces_old_groups() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 500, 400, 300, 0, 1),
            (2, H ^ 1, 4000, 1600, 1200, 0, 1),
        ],
    );
    run(&db);
    let p = run(&db);
    assert_eq!(p.groups, 1);
    assert_eq!(members_of(&db).len(), 2);
}

#[test]
fn rerunning_with_fewer_than_two_photos_clears_old_groups() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 500, 400, 300, 0, 1),
            (2, H ^ 1, 4000, 1600, 1200, 0, 1),
        ],
    );
    assert_eq!(run(&db).groups, 1);
    db.write(|c| c.execute("UPDATE files SET trashed_at=1 WHERE id=2", []))
        .unwrap();
    let p = run(&db);
    assert_eq!((p.groups, p.members), (0, 0));
    assert!(
        members_of(&db).is_empty(),
        "대상이 한 장뿐인데 이전 그룹이 남았다"
    );
}

#[test]
fn an_old_grayscale_signature_is_scheduled_for_recalculation() {
    let (d, db) = db();
    seed(&db, &[(1, H, 500, 400, 300, 0, 1)]);
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO libraries(id,volume_uuid,rel_path,name,area) VALUES(9,'V','','t',0)",
            [],
        )?;
        tx.execute("UPDATE folders SET library_id=9", [])?;
        tx.execute(
            "UPDATE files SET psig=?1 WHERE id=1",
            [vec![42u8; SIG * SIG]],
        )?;
        tx.execute(
            "INSERT INTO thumbs(file_id,rel_path,src_size,src_mtime,state)
                 VALUES(1,'old.jpg',500,0,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(jobs(&db, d.path()).unwrap().len(), 1);
}

#[test]
fn a_stale_thumbnail_is_never_used_to_recalculate_the_signature() {
    let (d, db) = db();
    seed(&db, &[(1, H, 500, 400, 300, 0, 1)]);
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO libraries(id,volume_uuid,rel_path,name,area) VALUES(9,'V','','t',0)",
            [],
        )?;
        tx.execute("UPDATE folders SET library_id=9", [])?;
        tx.execute(
            "UPDATE files SET phash=NULL, psig=NULL, modified_at=20 WHERE id=1",
            [],
        )?;
        tx.execute(
            "INSERT INTO thumbs(file_id,rel_path,src_size,src_mtime,state)
                 VALUES(1,'stale.jpg',500,10,1)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert!(jobs(&db, d.path()).unwrap().is_empty());

    db.write(|c| c.execute("UPDATE thumbs SET src_mtime=20 WHERE file_id=1", []))
        .unwrap();
    assert_eq!(jobs(&db, d.path()).unwrap().len(), 1);
}

#[test]
fn the_reason_says_what_is_kept_and_how_many_are_smaller() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 500, 400, 300, 0, 1),
            (2, H ^ 1, 4000, 1600, 1200, 0, 1),
        ],
    );
    run(&db);
    let reason: String = db
        .read(|c| c.query_row("SELECT reason FROM groups WHERE kind = 4", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(reason, "1600×1200 · 줄인 사본 1장");
}

/// 같은 해시가 아주 많아도 짝이 제곱으로 터지지 않는다 — 1단(같은 해시 뭉치기)이 하는 일.
#[test]
fn a_pile_of_identical_hashes_stays_one_group_without_exploding() {
    let (_d, db) = db();
    // 폴더를 갈라 둔다 — 한 폴더에 몰아 두면 연사로 보고 버린다
    let items: Vec<SeedItem> = (1..=300)
        .map(|i| (i, H, 100, 400, 300, (i % 3) as i32, 1))
        .collect();
    seed(&db, &items);
    let p = run(&db);
    assert_eq!(p.distinct, 1, "해시가 하나로 뭉쳐야 한다");
    assert_eq!(p.compared, 0, "견줄 짝이 없어야 한다");
    assert_eq!((p.groups, p.members), (1, 300));
}

/// 한 폴더 안 · 같은 해상도 = 연사. 실측에서 가장 큰 무리가 이것이었다
/// (`IMG_0040.CR2`~`IMG_0059.CR2`, 서로 다른 사진 11장).
#[test]
fn drops_a_burst_that_sits_in_one_folder_at_one_size() {
    let (_d, db) = db();
    // 해시까지 똑같은 연사 — 1단에서 뭉친 다음 여기서 걸러져야 한다
    seed(
        &db,
        &[
            (1, H, 3000, 5760, 3840, 0, 1),
            (2, H, 3100, 5760, 3840, 0, 1),
            (3, H, 3050, 5760, 3840, 0, 1),
        ],
    );
    let p = run(&db);
    assert_eq!((p.groups, p.bursts), (0, 1), "연사는 «같은 순간»이 맡는다");
    assert!(members_of(&db).is_empty());
}

/// 같은 해상도라도 **다른 폴더**에 있으면 사본이다 — 정리하다 두 자리에 남은 것
#[test]
fn keeps_a_same_size_copy_that_lives_in_another_folder() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 3000, 5760, 3840, 0, 1),
            (2, H, 3000, 5760, 3840, 2, 1),
        ],
    );
    let p = run(&db);
    assert_eq!((p.groups, p.bursts), (1, 0));
    assert_eq!(members_of(&db).len(), 2);
}

/// **해시가 이어도 그림이 다르면 안 묶는다.** 64비트 해시는 닮은 사진을 자주
/// 같은 값으로 낸다 — 화소 서명이 마지막 문지기다.
#[test]
fn a_matching_hash_over_a_different_picture_is_not_linked() {
    let (_d, db) = db();
    // 폴더도 크기도 갈라 둔다 — 오직 **서명**이 막는 것을 본다
    seed(
        &db,
        &[(1, H, 3000, 5760, 3840, 0, 1), (2, H, 300, 1440, 960, 2, 5)],
    );
    let p = run(&db);
    assert_eq!((p.groups, p.bursts), (0, 0));
}

/// 서명이 없는 사진은 아직 잴 수 없으니 대상에서 빠진다 — 잘못 묶는 것보다 낫다
#[test]
fn a_photo_without_a_signature_is_left_out() {
    let (_d, db) = db();
    seed(
        &db,
        &[(1, H, 500, 400, 300, 0, 1), (2, H, 4000, 1600, 1200, 0, 1)],
    );
    db.transaction(|tx| {
        tx.execute("UPDATE files SET psig = NULL WHERE id = 2", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(run(&db).groups, 0);
}

/// 반대로 **크기가 달라졌으면** 해시가 한두 비트 흔들려도 잇는다 — 이게 이 갈래의 뜻이다
#[test]
fn a_near_hash_at_a_different_size_is_a_resized_copy() {
    let (_d, db) = db();
    seed(
        &db,
        &[
            (1, H, 300, 1440, 960, 0, 1),
            (2, H ^ 3, 3000, 5760, 3840, 0, 1),
        ],
    );
    let p = run(&db);
    assert_eq!(p.groups, 1);
    assert_eq!(members_of(&db), vec![(1, false), (2, true)]);
}

/// A~B, B~C 라도 A 와 C 가 안 닮았으면 한 무리가 아니다 — 씨앗과 **직접** 닮아야 한다.
/// 실측에서 이 사슬이 하와이 이웃 컷 81장을 한 무리로 만들었다.
#[test]
fn does_not_chain_through_a_middle_picture() {
    let (_d, db) = db();
    // 서명 40 · 43 · 46 — 이웃끼리는 3(문턱 3.5 안), 양 끝은 6(밖)
    seed(
        &db,
        &[
            (1, H, 4000, 1600, 1200, 0, 1),
            (2, H, 500, 800, 600, 0, 1),
            (3, H, 300, 400, 300, 0, 1),
        ],
    );
    db.transaction(|tx| {
        tx.execute("UPDATE files SET psig = ?1 WHERE id = 2", [flat_sig(43)])?;
        tx.execute("UPDATE files SET psig = ?1 WHERE id = 3", [flat_sig(46)])?;
        Ok(())
    })
    .unwrap();
    let p = run(&db);
    // 씨앗은 가장 큰 1. 2는 닮아 들어오고, 3은 씨앗과 6 이나 떨어져 못 들어온다.
    assert_eq!((p.groups, p.members), (1, 2));
    assert_eq!(
        members_of(&db).iter().map(|m| m.0).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

/// 실제 라이브러리 사본으로 — 시간·무리 수·확보 용량을 본다.
/// `ACUT_DB_COPY=/path/copy.db ACUT_CACHE=<앱데이터> cargo test --release --lib cull::phash::tests::real -- --ignored --nocapture`
#[test]
#[ignore = "실제 DB 사본 필요"]
fn real_library_copy() {
    let Ok(path) = std::env::var("ACUT_DB_COPY") else {
        return;
    };
    let cache = std::env::var("ACUT_CACHE").unwrap_or_default();
    let db = Db::open(path).unwrap();
    let thr: u32 = std::env::var("ACUT_PHASH_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_THRESHOLD);
    let t = std::time::Instant::now();
    let p = scan(
        &db,
        Path::new(&cache),
        thr,
        Arc::new(AtomicBool::new(false)),
        |q| {
            if q.phase == "fill" && q.fill_done % 20_000 == 0 && q.fill_done > 0 {
                eprintln!("  해시 {}/{}", q.fill_done, q.fill_total);
            }
        },
    )
    .unwrap();
    eprintln!(
            "\n[줄인 사본] 문턱 {thr} · 해시 {}장(실패 {}) · 대상 {}장 · 서로 다른 해시 {} · 짝 {} · {}무리 {}장 · 연사로 버림 {}무리 · 확보 {:.1} GB · {:.1}초",
            p.fill_done, p.fill_failed, p.photos, p.distinct, p.compared,
            p.groups, p.members, p.bursts, p.reclaimable as f64 / 1024f64.powi(3), t.elapsed().as_secs_f64()
        );
    let sizes: Vec<i64> = db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT COUNT(*) FROM group_members m JOIN groups g ON g.id = m.group_id
                     WHERE g.kind = 4 GROUP BY g.id ORDER BY 1 DESC LIMIT 5",
            )?;
            let it = st.query_map([], |r| r.get(0))?;
            it.collect()
        })
        .unwrap();
    eprintln!("가장 큰 무리들: {sizes:?}");
}
