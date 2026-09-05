use super::*;

#[test]
fn progress_line_parses_percent_and_to_chk() {
    assert_eq!(
        parse_progress("     12,345,678  45%   12.34MB/s    0:00:05 (xfr#12, to-chk=88/200)"),
        Some((45, 112, 200))
    );
    assert_eq!(
        parse_progress("  1,000  3%  1.0MB/s  0:00:01"),
        Some((3, 0, 0))
    );
    assert_eq!(parse_progress("2024/여행/a.jpg"), None);
}

#[test]
fn explains_the_synology_rsync_wrapper_refusal() {
    let m = explain("Permission denied, please try again.\nrsync: connection unexpectedly closed");
    assert!(m.contains("DSM 제어판"));
    assert!(explain("ssh: Could not resolve hostname nasroot").contains("호스트가 없습니다"));
    assert_eq!(explain("  odd  "), "odd");
}

/// 실제 NAS — 작은 폴더 하나를 받아 본다. `ACUT_NAS_DIR=/volume1/.../_정리 cargo test --lib nas::ssh::tests::real -- --ignored --nocapture`
#[test]
#[ignore = "실제 NAS 필요"]
fn real_pull_small_folder() {
    let Ok(dir) = std::env::var("ACUT_NAS_DIR") else {
        return;
    };
    let cfg = Config {
        zone1: dir,
        ..Default::default()
    };
    // ACUT_PULL_DEST가 있으면 거기에(예: exFAT 볼륨 시험), 없으면 임시 폴더에
    let tmp = tempfile::tempdir().unwrap();
    let dest_override = std::env::var("ACUT_PULL_DEST")
        .ok()
        .map(std::path::PathBuf::from);
    let d = dest_override
        .clone()
        .unwrap_or_else(|| tmp.path().to_path_buf());
    struct D(std::path::PathBuf);
    impl Drop for D {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = dest_override.map(D);
    let d = D2(d);
    struct D2(std::path::PathBuf);
    impl D2 {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    let cancel = AtomicBool::new(false);
    let last = std::cell::RefCell::new(PullProgress::default());
    let t = std::time::Instant::now();
    let r = pull(&cfg, d.path(), &[], &cancel, |p| {
        *last.borrow_mut() = p.clone()
    })
    .unwrap();
    let last = last.into_inner();
    eprintln!(
        "\n받음 {}개 · 마지막 진행 {}/{} {}% · {:.1}초 · 취소 {}",
        r.files.len(),
        last.done,
        last.total,
        last.percent,
        t.elapsed().as_secs_f64(),
        r.cancelled
    );
    for f in r.files.iter().take(3) {
        eprintln!("  {f}");
    }
    assert!(!r.cancelled);
    let have = present_files(d.path()).len();
    eprintln!(
        "지금 있는 파일 {have}개 (이번에 옮긴 {}개 + 이미 있던 것)",
        r.files.len()
    );
    assert!(have >= r.files.len());
    // 두 번째는 받을 것이 없다 — 증분
    let r2 = pull(&cfg, d.path(), &[], &cancel, |_| {}).unwrap();
    assert_eq!(r2.files.len(), 0);
    // 원장에 있는 것은 로컬에서 지워도 다시 받지 않는다
    let first = present_files(d.path())[0].0.clone();
    std::fs::remove_file(d.path().join(&first)).unwrap();
    let (n, _) = count_new(&cfg, d.path(), std::slice::from_ref(&first)).unwrap();
    assert_eq!(n, 0, "원장에 있는 {first}는 새것이 아니다");
    let (n2, _) = count_new(&cfg, d.path(), &[]).unwrap();
    assert_eq!(n2, 1);
    let miss = missing_on_nas(&cfg, d.path(), &cfg.zone1).unwrap();
    eprintln!("확인: NAS에 없는 것 {}개 (0이어야)", miss.len());
    assert!(miss.is_empty());
}

#[test]
fn homebrew_rsync_is_preferred_and_openrsync_is_refused() {
    let (desc, ok) = rsync_version();
    // 이 맥에는 Homebrew rsync가 있다 — 없는 맥이면 openrsync라 false여야 한다
    assert_eq!(
        ok,
        !desc.contains("openrsync") && desc.starts_with("rsync"),
        "{desc}"
    );
}

#[test]
fn exclude_patterns_are_anchored_and_escaped() {
    assert_eq!(escape_pattern("a/b [1].jpg"), "a/b \\[1\\].jpg");
    assert_eq!(escape_pattern("what?.jpg"), "what\\?.jpg");
    let p = exclude_file(&["x/y.jpg".into(), "z*.png".into()])
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        "/x/y.jpg\n/z\\*.png\n"
    );
    let _ = std::fs::remove_file(p);
    assert!(exclude_file(&[]).unwrap().is_none());
}

#[test]
fn stats_lines_are_parsed() {
    let text = "Number of files: 2,701 (reg: 2,460, dir: 241)\nNumber of regular files transferred: 1,234\nTotal file size: 9,999 bytes\nTotal transferred file size: 12,345,678 bytes\n";
    assert_eq!(parse_stats(text), (1234, 12_345_678));
    assert_eq!(parse_stats(""), (0, 0));
}

#[test]
fn present_files_skips_the_partial_dir() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("a")).unwrap();
    std::fs::create_dir_all(d.path().join(PARTIAL_DIR)).unwrap();
    std::fs::write(d.path().join("a/x.jpg"), b"xx").unwrap();
    std::fs::write(d.path().join("a/._x.jpg"), b"sidecar").unwrap();
    std::fs::write(d.path().join(PARTIAL_DIR).join("y.jpg"), b"half").unwrap();
    let mut got = present_files(d.path());
    got.sort();
    assert_eq!(got, vec![("a/x.jpg".to_string(), 2)]);
}

#[test]
fn names_are_kept_in_nfc() {
    let nfd = "한글"
        .chars()
        .flat_map(|c| {
            use unicode_normalization::UnicodeNormalization;
            c.nfd().collect::<Vec<_>>()
        })
        .collect::<String>();
    assert_ne!(nfd, "한글");
    assert_eq!(nfc(&nfd), "한글");
}

#[test]
fn excludes_become_rsync_flags() {
    let cfg = Config {
        exclude: "@eaDir, #trash,,".into(),
        ..Default::default()
    };
    assert_eq!(
        excludes(&cfg),
        vec![
            "--exclude",
            "@eaDir",
            "--exclude",
            "#trash",
            "--exclude",
            "._*",
            "--exclude",
            PARTIAL_DIR
        ]
    );
}

#[test]
fn shell_quoting_survives_apostrophes() {
    assert_eq!(q("a'b"), "'a'\\''b'");
}

#[test]
fn read_until_any_splits_on_cr_and_lf() {
    let data = b"one\rtwo\nthree";
    let mut r = BufReader::new(&data[..]);
    let mut buf = Vec::new();
    read_until_any(&mut r, &mut buf).unwrap();
    assert_eq!(buf, b"one");
    buf.clear();
    read_until_any(&mut r, &mut buf).unwrap();
    assert_eq!(buf, b"two");
    buf.clear();
    read_until_any(&mut r, &mut buf).unwrap();
    assert_eq!(buf, b"three");
}

#[test]
fn partial_transfers_are_not_failures() {
    assert!(rsync_acceptable(true, Some(0), false));
    assert!(rsync_acceptable(false, Some(23), false));
    assert!(rsync_acceptable(false, Some(24), false));
    assert!(
        rsync_acceptable(false, Some(255), true),
        "멈춘 것은 실패가 아니다"
    );
    assert!(!rsync_acceptable(false, Some(255), false), "ssh 실패");
    assert!(!rsync_acceptable(false, Some(11), false), "파일 I/O 오류");
}

#[test]
fn purge_paths_stay_inside_zone1() {
    assert!(safe_zone1_rel("2024/여행/a.jpg"));
    assert!(!safe_zone1_rel("../Photos/a.jpg"));
    assert!(!safe_zone1_rel("/volume1/photo/a.jpg"));
    assert!(!safe_zone1_rel("a//b.jpg"));
    assert!(!safe_zone1_rel(""));
}
