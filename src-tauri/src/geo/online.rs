use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// 온라인에 물어본 결과 — **값의 출처와 다른 축이다.**
///
/// 서버가 이름을 못 찾았다고 해서 이미 가진 이름이 틀린 것은 아니다. 그래서 값은
/// 그대로 두고 여기에만 적는다. 이 기록이 없으면 같은 좌표를 볼 때마다 같은
/// 서버에 되풀이해 물어 «보강»이 영영 끝나지 않는다 (2026-09-01).
pub(super) const ONLINE_OK: &str = "success";
pub(super) const ONLINE_NONE: &str = "none";
pub(super) const ONLINE_SHALLOW: &str = "shallow";
pub(super) const ONLINE_CONFLICT: &str = "conflict";

/// 서버 답을 받아들일까 — 받아들이지 않으면 그 사유가 곧 조회 결과가 된다
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    Accept,
    /// 기존보다 얕거나, 국가 코드 없는 부분 응답이 검증된 국가를 바꾸려 한다
    Shallow,
    /// 국가가 내장 경계와 어긋난다 — 독도에 «일본»이 오는 것 같은 답
    Conflict,
}

impl Verdict {
    pub(super) fn outcome(self) -> &'static str {
        match self {
            Verdict::Accept => ONLINE_OK,
            Verdict::Shallow => ONLINE_SHALLOW,
            Verdict::Conflict => ONLINE_CONFLICT,
        }
    }
}

/// 서버 답을 받아들일지 판정한다.
///
/// 내장 경계가 나라를 아는 자리에서는 **경계가 이긴다.** 독도처럼 정책으로
/// 못 박은 좌표도 여기서 지켜진다 — 경계가 KR 이라고 답하기 때문이다.
/// 나라 이름 글월을 견주지 않는다. 서버가 어느 말로 답하느냐에 따라
/// «대한민국»과 «South Korea» 가 달라 보이기 때문이다 — ISO 두 글자로만 견준다.
pub(super) fn judge(
    boundary_cc: Option<&str>,
    answer_cc: Option<&str>,
    // 시도가 경계와 맞나 — `None` 은 «판단할 수 없다»이지 «어긋난다»가 아니다
    admin1_ok: Option<bool>,
    new_depth: u8,
    old_depth: u8,
) -> Verdict {
    if let Some(known) = boundary_cc {
        match answer_cc {
            // 나라가 어긋난다 — 값을 지키고 이 서버에는 다시 묻지 않는다
            Some(got) if !got.eq_ignore_ascii_case(known) => return Verdict::Conflict,
            // 나라를 밝히지 않은 답은 검증된 나라를 바꿀 수 없다
            None => return Verdict::Shallow,
            _ => {}
        }
    }
    // 나라가 같아도 도가 틀릴 수 있다. 격자 대표 좌표는 칸 안 어딘가일 뿐이라,
    // 도 경계에 걸친 칸에서 서버가 옆 도를 답하는 일이 실제로 생긴다.
    // 우리가 아는 시도 이름과 어긋날 때만 막는다 — 모르는 이름에는 다투지 않는다.
    if admin1_ok == Some(false) {
        return Verdict::Conflict;
    }
    // 시군구까지 있던 자리를 나라만 있는 답으로 바꾸면 두 단계를 잃는다
    if new_depth < old_depth {
        return Verdict::Shallow;
    }
    Verdict::Accept
}

/// 주소 조각에서 ISO 3166-1 alpha-2 를 꺼낸다.
///
/// Nominatim 은 소문자로 준다(`"kr"`). 두 글자 알파벳이 아니면 믿지 않는다 —
/// 어떤 서버는 `"gb-eng"` 같은 값을 넣거나 칸을 아예 빼기도 한다.
pub fn country_code(addr: &serde_json::Value) -> Option<String> {
    let raw = addr.get("country_code")?.as_str()?.trim();
    let up = raw.to_ascii_uppercase();
    if up.len() == 2 && up.bytes().all(|b| b.is_ascii_alphabetic()) {
        Some(up)
    } else {
        None
    }
}

/// 물어본 결과 — 넷을 갈라야 «결과 없음»·«잠깐 실패»·«설정이 틀림»을 다르게 다룬다.
pub(super) enum Answer {
    Found(Found),
    /// 그 자리에 이름이 없다 — 캐시에 못 박고 다시 묻지 않는다
    Nothing,
    /// 잠깐 실패했다 — 조금 쉬었다 다시 물으면 된다 (5xx · 429 · 연결 끊김)
    Retryable {
        message: String,
        retry_after: Option<Duration>,
    },
    /// 다시 물어도 소용없다 — 주소나 권한이 틀렸다. 캐시하지 않고 멈춘다.
    Fatal(String),
}

/// 서버가 준 한 자리의 답 — 이름과, 그 이름이 어느 나라 것인지.
///
/// 나라 코드를 따로 나르는 이유: 이름은 서버 언어에 따라 달라지지만 ISO 두
/// 글자는 그렇지 않다. 내장 경계와 견주려면 흔들리지 않는 열쇠가 있어야 한다.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Found {
    pub(super) place: Place,
    /// ISO 3166-1 alpha-2 대문자. 서버가 밝히지 않았으면 None.
    pub(super) cc: Option<String>,
}

/// 재시도 사이에 쉬는 시간 — 2초, 5초, 15초. 서버가 Retry-After 를 주면 그것을 따른다.
/// 어느 값도 `GAP`(초당 한 건)보다 짧지 않다 — 재시도가 그 약속을 깨면 안 된다.
pub(super) const RETRIES: &[Duration] = &[
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
];
/// 서버가 «한참 뒤에 오라»고 해도 이보다 오래 붙잡고 있지 않는다
const RETRY_CAP: Duration = Duration::from_secs(30);

/// Retry-After 머리글을 읽는다 — 초 단위 숫자만 받는다(날짜 형식은 무시)
pub(super) fn retry_after(res: &reqwest::blocking::Response) -> Option<Duration> {
    let raw = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(RETRY_CAP))
}

/// 취소를 살피며 쉰다 — 4초 백오프 중에 «그만»을 눌러도 곧바로 멈추게
pub(super) fn nap(cancel: &AtomicBool, total: Duration) -> bool {
    let step = Duration::from_millis(100);
    let mut left = total;
    while left > Duration::ZERO {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let d = step.min(left);
        std::thread::sleep(d);
        left -= d;
    }
    !cancel.load(Ordering::Relaxed)
}

/// 잠깐 실패면 세 번까지 다시 묻는다. 그 밖의 답은 그대로 돌려준다.
pub(super) fn ask_with_retry(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    lat: f64,
    lon: f64,
    zoom: u8,
    cancel: &AtomicBool,
) -> Answer {
    let mut last = Answer::Fatal("지명 서버에 묻지 못했습니다".into());
    for (attempt, backoff) in RETRIES.iter().enumerate() {
        match ask(client, endpoint, lat, lon, zoom) {
            Answer::Retryable {
                message,
                retry_after,
            } => {
                // 서버가 «곧 와도 된다»고 해도 초당 한 건은 지킨다
                let wait = retry_after.unwrap_or(*backoff).max(GAP);
                log::warn!(
                    "지명 조회 재시도 {}/{}: {message} ({}초 뒤)",
                    attempt + 1,
                    RETRIES.len(),
                    wait.as_secs()
                );
                last = Answer::Retryable {
                    message,
                    retry_after,
                };
                if attempt + 1 == RETRIES.len() || !nap(cancel, wait) {
                    break;
                }
            }
            other => return other,
        }
    }
    last
}

/// 좌표 하나를 물어본다.
pub(super) fn ask(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    lat: f64,
    lon: f64,
    zoom: u8,
) -> Answer {
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    let url =
        format!("{endpoint}{sep}lat={lat}&lon={lon}&format=jsonv2&zoom={zoom}&accept-language=ko");
    let res = match client
        .get(&url)
        .header(reqwest::header::USER_AGENT, UA)
        .send()
    {
        Ok(r) => r,
        // 연결·시간 초과는 망 사정일 때가 많다 — 다시 물어볼 값어치가 있다
        // `without_url()` 로 주소를 뗀다 — 주소에 인증 토큰이 들어 있으면
        // 오류 글월을 타고 로그 파일에 남는다
        Err(e) => {
            return Answer::Retryable {
                message: format!("지명 서버에 연결하지 못했습니다: {}", e.without_url()),
                retry_after: None,
            }
        }
    };
    let status = res.status();
    if !status.is_success() {
        let after = retry_after(&res);
        // 429(너무 잦음)와 5xx(서버 사정)는 기다리면 풀린다.
        // 그 밖의 4xx 는 주소나 권한이 틀린 것이라 다시 물어도 같은 답이 온다.
        return if status.as_u16() == 429 || status.is_server_error() {
            Answer::Retryable {
                message: format!("서버가 {status} 로 답했습니다"),
                retry_after: after,
            }
        } else {
            Answer::Fatal(format!(
                "서버가 {status} 로 답했습니다 — 설정 › 탐색의 지명 서버 주소를 확인해 주세요"
            ))
        };
    }
    let body: serde_json::Value = match res.json() {
        Ok(v) => v,
        // 본문이 깨진 것은 대개 중간 장비가 끼어든 경우다 — 한 번 더 물어본다
        Err(e) => {
            return Answer::Retryable {
                message: format!("지명 서버 응답을 읽지 못했습니다: {}", e.without_url()),
                retry_after: None,
            }
        }
    };
    // 서버가 200 으로 오류를 싣는 경우도 있다
    if let Some(error) = body.get("error") {
        let message = error
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        let lower = message.to_ascii_lowercase();
        return if lower.contains("unable to geocode")
            || lower.contains("not found")
            || lower.contains("no result")
        {
            Answer::Nothing
        } else {
            Answer::Fatal(format!("지명 서버 오류: {message}"))
        };
    }
    let addr = body.get("address");
    let place = addr.map(fold).unwrap_or_default();
    let cc = addr.and_then(country_code);
    // 위치 트리의 첫 단계이자 처리 완료 표시는 국가다. 국가가 없는 부분 응답을
    // 성공 캐시로 남기면 같은 파일이 영원히 미완료로 남는다.
    if place.country.is_none() {
        Answer::Nothing
    } else {
        Answer::Found(Found { place, cc })
    }
}
