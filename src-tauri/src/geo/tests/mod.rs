use super::fill::{
    cache_cells_left, current_depth, provider_of, settle_empty, targets, write_place, Overwrite,
};
use super::online::{
    ask, ask_with_retry, judge, nap, Answer, Verdict, ONLINE_CONFLICT, ONLINE_NONE, ONLINE_OK,
    ONLINE_SHALLOW, RETRIES,
};
use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 시험용 최소 HTTP 서버 — 미리 준비한 응답을 순서대로 하나씩 돌려준다.
///
/// 안전장치(2026-09-01 리뷰): 클라이언트가 오지 않아도 **스스로 끝난다**.
/// nonblocking accept + 2초 마감 + 소켓 읽기·쓰기 시간 제한. 시험은 join 대신
/// 채널을 recv_timeout 으로 받아 «실패»가 «영원한 대기»가 되지 않게 한다.
pub(super) struct TestServer {
    url: String,
    /// 스레드가 끝나며 «받은 요청 수»를 보낸다
    done: std::sync::mpsc::Receiver<usize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    /// `replies` 는 (상태줄, 본문, 여분 헤더) 목록 — 요청 순서대로 쓰인다.
    /// 목록이 다 떨어지면 마지막 것을 되풀이한다.
    pub(super) fn start(replies: Vec<(&'static str, &'static str, Option<&'static str>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/reverse", listener.local_addr().unwrap());
        let (tx, done) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            let mut served = 0usize;
            while std::time::Instant::now() < deadline && !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0_u8; 2048];
                        let _ = stream.read(&mut buf);
                        let (status, body, extra) = replies
                            .get(served)
                            .copied()
                            .unwrap_or_else(|| *replies.last().unwrap());
                        served += 1;
                        let extra = extra.map(|h| format!("{h}\r\n")).unwrap_or_default();
                        let res = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                        let _ = stream.write_all(res.as_bytes());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(served);
        });
        TestServer {
            url,
            done,
            stop,
            handle: Some(handle),
        }
    }

    /// 응답을 한 번만 주는 서버 — 흔한 경우
    pub(super) fn once(status: &'static str, body: &'static str) -> Self {
        Self::start(vec![(status, body, None)])
    }

    /// 서버가 받은 요청 수 — 재시도가 실제로 일어났는지 센다
    pub(super) fn served(&mut self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        let n = self
            .done
            .recv_timeout(Duration::from_secs(3))
            .expect("서버 스레드가 끝나야 한다");
        if let Some(h) = self.handle.take() {
            h.join().expect("서버 스레드 join");
        }
        n
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // 시험이 중간에 실패해도 스레드를 남기지 않는다
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 시험용 클라이언트 — 반드시 시간 제한을 둔다. 없으면 실패가 무한 대기가 된다
pub(super) fn test_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_millis(500))
        .redirect(reqwest::redirect::Policy::none())
        // CI 나 macOS 의 프록시 환경 변수가 127.0.0.1 요청을 가로채지 않게
        .no_proxy()
        .build()
        .unwrap()
}

/// 사진 몇 장과 좌표만 있는 최소한의 DB
pub(super) fn db_with(coords: &[(i64, f64, f64)]) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.db")).unwrap();
    db.write(|c| {
        c.execute_batch(
            "INSERT INTO volumes(uuid,name,role) VALUES('V','t','library');
                 INSERT INTO libraries(id,volume_uuid,rel_path,name,area) VALUES(1,'V','a','a',1);
                 INSERT INTO folders(id,volume_uuid,library_id,rel_path,name,area)
                   VALUES(1,'V',1,'a','a',1);",
        )
    })
    .unwrap();
    for (id, lat, lon) in coords {
        db.write(|c| {
                c.execute(
                    "INSERT INTO files(id,folder_id,name,size,kind,taken_at,taken_at_source,scanned_at,gps_lat,gps_lon)
                     VALUES(?1,1,?2,1,0,1,0,0,?3,?4)",
                    rusqlite::params![id, format!("f{id}.jpg"), lat, lon],
                )
            })
            .unwrap();
    }
    (dir, db)
}

pub(super) fn geo_of(
    db: &Db,
    id: i64,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    db.read(|c| {
        c.query_row(
            "SELECT geo_country, geo_admin1, geo_admin2, geo_name FROM files WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
    })
    .unwrap()
}

pub(super) fn set_endpoint(db: &Db, url: &str) {
    crate::db::settings::set(db, "geo.endpoint", url).unwrap();
}

mod client;
mod fill_a;
mod fill_b;
mod fill_c;
mod place;
