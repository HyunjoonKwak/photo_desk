//! NAS — SSH와 rsync로. DSM API가 아니라 배관공의 도구다.
//!
//! 이 맥에는 `ssh nas`(포트 72, 키)가 이미 있고 NAS에는 rsync가 있다.
//! 내려받기·확인·비우기 셋 다 rsync/ssh 한 줄로 끝나고, 18,500개를 하나씩
//! HTTPS로 부르는 것보다 훨씬 빠르며 끊겨도 이어받는다. 자격증명은 저장하지
//! 않는다 — ssh 설정이 든다.
//!
//! rsync는 DSM의 rsync용 sshd(포트 22)로만 받아 준다 — 일반 sshd(72)로 들어온
//! rsync는 setuid 래퍼가 «Permission denied»를 낸다. 그래서 rsync는 `-p 22`,
//! 셸 명령(확인·비우기)은 ssh 별칭(72)으로 간다. 22번 sshd는 셸을 안 준다.
//! known_hosts에서 22번은 `[host]:22`가 아니라 맨 호스트명으로 찾는다 —
//! 같은 호스트 키를 이미 믿고 있으니 처음 보는 이름은 받아들인다(accept-new).
//!
//! DSM의 휴지통(#recycle)은 이 NAS에서 꺼져 있다. 그래서 «삭제»는 1차 구역
//! 안의 `#trash/`로 옮기는 것이다 — nas_photo가 공용에 쓰는 것과 같은 이름.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// ssh 설정의 Host 이름
    pub host: String,
    /// 1차 구역 — 폰·가족·구글포토가 먼저 닿는 곳. 처리 대기열.
    pub zone1: String,
    /// 개인(2차) — 맥 «내사진»과 1:1
    pub photos: String,
    /// 공용 — 맥 «공용»과 1:1
    pub shared: String,
    /// 내려받을 때 빼는 것 — 쉼표로
    pub exclude: String,
    /// rsync가 붙는 SSH 포트. DSM의 rsync 서비스는 제 sshd(기본 22)로만 받는다 —
    /// 일반 sshd(이 맥은 72)로 들어온 rsync는 setuid 래퍼가 거절한다 (실측).
    #[serde(default = "default_rsync_port")]
    pub rsync_port: u16,
}

fn default_rsync_port() -> u16 {
    22
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: "nas".into(),
            zone1: "/volume1/homes/luckyguy/Personal".into(),
            photos: "/volume1/homes/luckyguy/Photos".into(),
            shared: "/volume1/photo".into(),
            exclude: "@eaDir,#recycle,#trash,_quarantine,.DS_Store,Thumbs.db".into(),
            rsync_port: 22,
        }
    }
}

/// 1차 구역 안의 휴지통 폴더 이름
pub const TRASH_DIR: &str = "#trash";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub online: bool,
    pub hostname: String,
    pub free_bytes: Option<u64>,
    pub zone1_files: Option<u64>,
    pub error: Option<String>,
    /// 이 맥의 rsync — 경로와 판. macOS 내장 openrsync는 옵션이 달라 못 쓴다.
    pub rsync: String,
    pub rsync_ok: bool,
}

/// 쓸 rsync — Homebrew 것을 먼저. GUI 앱의 PATH에는 /opt/homebrew/bin이 없어
/// 그냥 `rsync`라고 하면 macOS 내장 openrsync(프로토콜 29)가 잡힌다.
pub fn rsync_bin() -> std::path::PathBuf {
    for p in ["/opt/homebrew/bin/rsync", "/usr/local/bin/rsync"] {
        if Path::new(p).is_file() {
            return Path::new(p).to_path_buf();
        }
    }
    std::path::PathBuf::from("rsync")
}

/// (설명, 쓸 수 있나)
pub fn rsync_version() -> (String, bool) {
    let bin = rsync_bin();
    match Command::new(&bin).arg("--version").output() {
        Ok(o) => {
            let first = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let ok = first.starts_with("rsync") && !first.contains("openrsync");
            (format!("{first} — {}", bin.display()), ok)
        }
        Err(e) => (format!("rsync 없음 ({e})"), false),
    }
}

/// rsync가 남긴 stderr를 사람 말로. Synology의 /usr/bin/rsync는 setuid 래퍼라
/// ssh 인증이 됐어도 DSM의 rsync 서비스·사용자 권한이 없으면 «Permission denied»를 낸다.
pub fn explain(stderr: &str) -> String {
    let t = stderr.trim();
    if t.contains("Permission denied") {
        return "NAS가 rsync를 거절했습니다 — DSM 제어판 › 파일 서비스 › rsync에서 «rsync 서비스 사용»을 켜고, 설정의 rsync 포트가 DSM의 rsync용 SSH 포트(기본 22)와 같은지 보세요. (ssh 접속 자체는 됩니다)".into();
    }
    if t.contains("Could not resolve hostname") {
        return format!("ssh 설정에 그 호스트가 없습니다: {t}");
    }
    if t.contains("Connection timed out")
        || t.contains("Connection refused")
        || t.contains("No route to host")
    {
        return format!("NAS에 닿지 않습니다 — 켜져 있고 같은 네트워크인지 보세요: {t}");
    }
    t.to_string()
}

fn ssh_base(cfg: &Config) -> Command {
    let mut c = Command::new("ssh");
    c.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "LogLevel=ERROR",
        &cfg.host,
    ]);
    c
}

/// 셸에 넣을 경로 — 작은따옴표로 감싼다
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 연결이 되나, 남은 공간과 1차 구역의 파일 수는
pub fn check(cfg: &Config) -> Status {
    let (rsync, rsync_ok) = rsync_version();
    let script = format!(
        "hostname; df -Pk {z} | tail -1 | awk '{{print $4}}'; find {z} -type f ! -path '*/@eaDir/*' ! -path '*/{t}/*' ! -path '*/#recycle/*' | wc -l",
        z = q(&cfg.zone1),
        t = TRASH_DIR
    );
    let out = ssh_base(cfg).arg(script).output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut lines = text.lines().map(str::trim);
            let hostname = lines.next().unwrap_or("").to_string();
            let free_bytes = lines
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .map(|kb| kb * 1024);
            let zone1_files = lines.next().and_then(|s| s.parse().ok());
            Status {
                online: true,
                hostname,
                free_bytes,
                zone1_files,
                error: None,
                rsync,
                rsync_ok,
            }
        }
        Ok(o) => Status {
            online: false,
            hostname: String::new(),
            free_bytes: None,
            zone1_files: None,
            error: Some(explain(&String::from_utf8_lossy(&o.stderr))),
            rsync,
            rsync_ok,
        },
        Err(e) => Status {
            online: false,
            hostname: String::new(),
            free_bytes: None,
            zone1_files: None,
            error: Some(e.to_string()),
            rsync,
            rsync_ok,
        },
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PullProgress {
    /// 옮긴 항목 / 전체 항목 (rsync의 to-chk)
    pub done: usize,
    pub total: usize,
    pub percent: u8,
    pub current: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Pulled {
    /// 새로 받은 파일들 — 상대경로
    pub files: Vec<String>,
    pub cancelled: bool,
}

/// 파일을 견주고 옮기는 방식 — `-a`가 아니다. 목적지가 exFAT(옛 백업 SSD)이면
/// 권한·소유자를 못 맞춰 파일마다 stderr에 한 줄씩 남기고, 시각은 2초 단위라
/// 매번 «바뀌었다»고 본다. 재귀·링크·시각만 맞추고 2초 오차는 눈감는다.
/// `--iconv=utf-8-mac,utf-8`: 맥은 한글 이름을 자모 분리(NFD)로 두고 NAS는 NFC라, 그냥
/// 견주면 같은 이름이 다른 파일이 된다 (실측: 2,460장 가운데 1,592장이 «NAS에 없음»).
const COPY_FLAGS: [&str; 6] = [
    "-rlt",
    "--no-perms",
    "--no-owner",
    "--no-group",
    "--modify-window=2",
    "--iconv=utf-8-mac,utf-8",
];

/// 우리 쪽 기록은 전부 NFC — DB의 파일·폴더 이름과 같은 꼴
fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}
/// 받다 만 파일은 여기에 — 완성된 것만 제 이름으로 보인다
pub const PARTIAL_DIR: &str = ".rsync-partial";

/// rsync가 쓸 ssh 명령 — rsync용 포트로
fn rsync_ssh(cfg: &Config) -> String {
    format!(
        "ssh -p {} -o BatchMode=yes -o LogLevel=ERROR -o StrictHostKeyChecking=accept-new",
        cfg.rsync_port
    )
}

/// 사용자가 뭐라 적었든 늘 빼는 것 — macOS가 exFAT에 만드는 `._` 사이드카(실측: 2,460장에
/// 2,460개가 «NAS에 없는 것»으로 잡혔다)와 받다 만 파일 폴더.
const ALWAYS_EXCLUDE: [&str; 2] = ["._*", PARTIAL_DIR];

fn excludes(cfg: &Config) -> Vec<String> {
    cfg.exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .chain(ALWAYS_EXCLUDE.iter().copied())
        .flat_map(|p| ["--exclude".to_string(), p.to_string()])
        .collect()
}

/// rsync 진행 줄 — `12,345  45%  1.2MB/s  0:00:03 (xfr#12, to-chk=88/200)`
fn parse_progress(line: &str) -> Option<(u8, usize, usize)> {
    let pct = line
        .split_whitespace()
        .find(|w| w.ends_with('%'))?
        .trim_end_matches('%')
        .parse()
        .ok()?;
    let (done, total) = match line.find("to-chk=") {
        Some(i) => {
            let rest = &line[i + 7..];
            let end = rest.find(')').unwrap_or(rest.len());
            let mut it = rest[..end].split('/');
            let left: usize = it.next()?.trim().parse().ok()?;
            let total: usize = it.next()?.trim().parse().ok()?;
            (total.saturating_sub(left), total)
        }
        None => (0, 0),
    };
    Some((pct, done, total))
}

/// rsync 패턴에서 뜻을 가지는 글자를 막는다 — 파일 이름은 이름 그대로여야 한다
fn escape_pattern(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len() + 4);
    for c in rel.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 이미 받은 것들(원장)을 rsync가 건너뛰게 — `--exclude-from` 파일.
/// 내려받은 사진을 고르고 내사진으로 옮기면 작업대에서 사라지는데, 이게 없으면
/// 다음 내려받기가 같은 사진을 또 받는다. 없으면 None.
fn exclude_file(already: &[String]) -> std::io::Result<Option<std::path::PathBuf>> {
    if already.is_empty() {
        return Ok(None);
    }
    let p = std::env::temp_dir().join(format!("acut-nas-exclude-{}.txt", std::process::id()));
    let body: String = already
        .iter()
        .map(|r| format!("/{}\n", escape_pattern(r)))
        .collect();
    std::fs::write(&p, body)?;
    Ok(Some(p))
}

/// 지금 폴더에 있는 완성 파일들 — 상대경로와 크기. 받다 만 것(.rsync-partial)은 뺀다.
/// 원장에 넣을 때 쓴다: 멈췄다 이어받아도, 처음부터 있었어도, 있으면 «받은 것»이다.
pub fn present_files(dest: &Path) -> Vec<(String, u64)> {
    walkdir::WalkDir::new(dest)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name().to_string_lossy() != PARTIAL_DIR)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && !e.file_name().to_string_lossy().starts_with("._"))
        .filter_map(|e| {
            let rel = nfc(&e.path().strip_prefix(dest).ok()?.to_string_lossy());
            let size = e.metadata().ok()?.len();
            Some((rel, size))
        })
        .collect()
}

/// 폴더 아래 **사진·영상** 파일 수 — 확인(verify)의 «디스크가 다 붙었나» 판단용.
/// `.DS_Store`·사이드카·`@eaDir` 안의 것은 DB 행이 아니니 세면 안 된다 (리뷰 C7)
pub fn present_media_count(dest: &Path) -> usize {
    walkdir::WalkDir::new(dest)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !(e.file_type().is_dir() && crate::scan::kinds::is_skipped_dir(&n)) && n != PARTIAL_DIR
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| crate::scan::kinds::classify(&e.file_name().to_string_lossy()).is_some())
        .count()
}

/// 1차 구역에 «받은 적 없는 것»이 몇 개·몇 바이트나 — rsync 시험 실행(-n --stats).
pub fn count_new(cfg: &Config, dest: &Path, already: &[String]) -> std::io::Result<(usize, u64)> {
    let (_, ok) = rsync_version();
    if !ok {
        return Err(std::io::Error::other("이 맥의 rsync가 macOS 내장 openrsync라 쓸 수 없습니다 — 터미널에서 `brew install rsync` 뒤 다시 하세요"));
    }
    let src = format!("{}:{}/", cfg.host, cfg.zone1.trim_end_matches('/'));
    let excl = exclude_file(already)?;
    let mut cmd = Command::new(rsync_bin());
    cmd.args(COPY_FLAGS)
        .args(["-n", "--stats", "-e", &rsync_ssh(cfg)]);
    cmd.args(excludes(cfg));
    if let Some(p) = &excl {
        cmd.arg(format!("--exclude-from={}", p.display()));
    }
    // 아직 없는 폴더라도 셀 수는 있어야 한다 — 빈 임시 폴더와 견준다
    let local = if dest.is_dir() {
        dest.to_path_buf()
    } else {
        std::env::temp_dir().join("acut-nas-empty")
    };
    std::fs::create_dir_all(&local)?;
    cmd.arg(&src).arg(format!("{}/", local.to_string_lossy()));
    let out = cmd.output();
    if let Some(p) = excl {
        let _ = std::fs::remove_file(p);
    }
    let out = out?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "rsync 실패: {}",
            explain(&String::from_utf8_lossy(&out.stderr))
        )));
    }
    Ok(parse_stats(&String::from_utf8_lossy(&out.stdout)))
}

/// `--stats` 출력에서 (옮길 파일 수, 바이트)
fn parse_stats(text: &str) -> (usize, u64) {
    let num = |line: &str| -> u64 {
        line.split(':')
            .nth(1)
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .replace(',', "")
            .parse()
            .unwrap_or(0)
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for l in text.lines() {
        if l.starts_with("Number of regular files transferred:") {
            files = num(l) as usize;
        } else if l.starts_with("Total transferred file size:") {
            bytes = num(l);
        }
    }
    (files, bytes)
}

/// 1차 구역을 `dest`로 내려받는다. 이어받기·증분은 rsync가 한다.
/// `already`(원장)에 있는 것은 건너뛴다 — 작업대에서 정리돼 나간 사진을 또 받지 않게.
pub fn pull(
    cfg: &Config,
    dest: &Path,
    already: &[String],
    cancel: &AtomicBool,
    on_progress: impl Fn(&PullProgress),
) -> std::io::Result<Pulled> {
    std::fs::create_dir_all(dest)?;
    let excl = exclude_file(already)?;
    let src = format!("{}:{}/", cfg.host, cfg.zone1.trim_end_matches('/'));
    let (_, ok) = rsync_version();
    if !ok {
        return Err(std::io::Error::other("이 맥의 rsync가 macOS 내장 openrsync라 쓸 수 없습니다 — 터미널에서 `brew install rsync` 뒤 다시 하세요"));
    }
    let mut cmd = Command::new(rsync_bin());
    cmd.args(COPY_FLAGS).args([
        "--partial",
        &format!("--partial-dir={PARTIAL_DIR}"),
        "--no-inc-recursive",
        "--info=progress2",
        "--out-format=%n",
        "-e",
        &rsync_ssh(cfg),
    ]);
    cmd.args(excludes(cfg));
    if let Some(p) = &excl {
        cmd.arg(format!("--exclude-from={}", p.display()));
    }
    cmd.arg(&src).arg(format!("{}/", dest.to_string_lossy()));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().unwrap();
    // stderr는 다른 스레드가 비운다. 안 읽으면 파이프(64KB)가 차는 순간 rsync가
    // 멎고, 우리는 stdout을 기다리며 같이 멎는다 — 실측: exFAT에서 5,358장에서.
    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut s = String::new();
        let mut r = stderr;
        let _ = r.read_to_string(&mut s);
        s
    });
    let mut out = Pulled::default();
    let mut p = PullProgress::default();
    // rsync는 진행 줄을 \r로 덮어쓴다 — \r과 \n 둘 다 줄 끝으로 본다
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            out.cancelled = true;
            break;
        }
        buf.clear();
        let n = read_until_any(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some((pct, done, total)) = parse_progress(&line) {
            p.percent = pct;
            if total > 0 {
                p.done = done;
                p.total = total;
            }
            on_progress(&p);
        } else if !line.ends_with('/') {
            let line = nfc(&line);
            p.current = line.clone();
            out.files.push(line);
        }
    }
    let status = child.wait()?;
    if let Some(p) = excl {
        let _ = std::fs::remove_file(p);
    }
    let err = err_thread.join().unwrap_or_default();
    if !rsync_acceptable(status.success(), status.code(), out.cancelled) {
        return Err(std::io::Error::other(format!(
            "rsync 실패 ({status}): {}",
            explain(&err)
        )));
    }
    if !status.success() {
        // 23·24 — 옮기는 사이 NAS 쪽이 바뀌었거나 몇 개를 못 읽었다. 받은 건 받은 것이다.
        log::warn!("rsync 가 일부만 옮겼습니다 ({status}): {}", explain(&err));
    }
    Ok(out)
}

/// rsync 종료 코드 판정 — 0 은 성공, 23(일부 실패)·24(옮기는 사이 사라짐)는 살아 있는
/// NAS(Drive 가 같이 쓰는)에서 흔한 «일부만»이라 실패로 보지 않는다. 그렇게 보면
/// 받은 파일이 원장에 안 올라 다음에 또 받고, 1차 비우기 후보에서도 빠진다 (리뷰 H10).
pub fn rsync_acceptable(success: bool, code: Option<i32>, cancelled: bool) -> bool {
    success || cancelled || matches!(code, Some(23) | Some(24))
}

/// 1차 구역 기준 상대경로로 안전한가 — 원장에 없거나 위로 올라가는 경로는 거른다.
/// NAS 쪽 스크립트는 `#trash/$f` 로 옮길 뿐 경로를 가두지 않는다 (리뷰 C6).
pub fn safe_zone1_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
}

/// \r 또는 \n까지 읽는다
fn read_until_any(r: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok(total);
        }
        let pos = avail.iter().position(|&b| b == b'\r' || b == b'\n');
        match pos {
            Some(i) => {
                buf.extend_from_slice(&avail[..i]);
                r.consume(i + 1);
                return Ok(total + i + 1);
            }
            None => {
                let n = avail.len();
                buf.extend_from_slice(avail);
                r.consume(n);
                total += n;
            }
        }
    }
}

/// 로컬 폴더가 NAS 폴더에 다 있나 — 없거나 크기가 다른 파일의 상대경로.
/// rsync 시험 실행(-n): «보낼 것»이 곧 «NAS에 없는 것»이다. 실제로는 아무것도 안 보낸다.
pub fn missing_on_nas(cfg: &Config, local: &Path, remote: &str) -> std::io::Result<Vec<String>> {
    let (_, ok) = rsync_version();
    if !ok {
        return Err(std::io::Error::other("이 맥의 rsync가 macOS 내장 openrsync라 쓸 수 없습니다 — 터미널에서 `brew install rsync` 뒤 다시 하세요"));
    }
    let out = Command::new(rsync_bin())
        .args(COPY_FLAGS)
        .args([
            "-n",
            "--size-only",
            "--out-format=%n",
            "-e",
            &rsync_ssh(cfg),
        ])
        .args(excludes(cfg))
        .arg(format!("{}/", local.to_string_lossy()))
        .arg(format!("{}:{}/", cfg.host, remote.trim_end_matches('/')))
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "rsync 실패: {}",
            explain(&String::from_utf8_lossy(&out.stderr))
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with('/') && *l != "./")
        .map(nfc)
        .collect())
}

/// 1차 구역의 파일들을 그 안의 `#trash/`로 옮긴다. 옮긴 것의 상대경로를 돌려준다.
/// 옮긴 것과, 있었다면 실패 사유. 몇 개가 실패해도 옮긴 것은 돌려준다 — 호출자가 원장을
/// 그만큼은 지워야 다음 비우기에 «이미 없는 파일»이 다시 안 나온다 (리뷰 H8)
pub struct Trashed {
    pub moved: Vec<String>,
    pub error: Option<String>,
}

pub fn trash_in_zone1(cfg: &Config, rels: &[String]) -> std::io::Result<Trashed> {
    if rels.is_empty() {
        return Ok(Trashed {
            moved: Vec::new(),
            error: None,
        });
    }
    // 목록은 stdin으로 NUL 구분 — 이름에 무엇이 들어 있어도 된다.
    // 이름은 mv 가 진짜 성공했을 때만 찍는다. `mv -n` 은 목적지에 같은 이름이 있으면
    // 옮기지 않고도 0 으로 끝나 «옮겼다»고 적히던 길 — 그 파일은 NAS 에 그대로였다.
    let script = format!(
        "cd {z} || exit 3; fail=0; while IFS= read -r -d '' f; do \
           if [ ! -e \"$f\" ]; then continue; fi; \
           if [ -e \"{t}/$f\" ]; then fail=1; echo \"이미 휴지통에 있음: $f\" >&2; continue; fi; \
           d=$(dirname \"$f\"); \
           if mkdir -p \"{t}/$d\" && mv -- \"$f\" \"{t}/$f\"; then printf '%s\\0' \"$f\"; else fail=1; fi; \
         done; exit $fail",
        z = q(&cfg.zone1),
        t = TRASH_DIR
    );
    let mut child = ssh_base(cfg)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in rels {
            stdin.write_all(r.as_bytes())?;
            stdin.write_all(&[0])?;
        }
    }
    let out = child.wait_with_output()?;
    let moved: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if out.status.code() == Some(3) {
        return Err(std::io::Error::other(format!(
            "NAS 의 1차 구역에 들어갈 수 없습니다: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let error = (!out.status.success()).then(|| {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if e.is_empty() {
            "일부를 못 옮겼습니다".to_string()
        } else {
            e
        }
    });
    Ok(Trashed { moved, error })
}

#[cfg(test)]
mod tests;
