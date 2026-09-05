//! 볼륨 식별 — 절대경로 대신 "볼륨 UUID + 볼륨 내 상대경로"로 파일을 가리킨다.
//!
//! 왜 필요한가: macOS의 마운트 경로는 안정적이지 않다. 같은 이름의 볼륨이 이미
//! 붙어 있으면 뒤에 오는 쪽이 `PHOTO 1`처럼 밀린다. 실제로 이 프로젝트에서
//! 로컬 SSD `PHOTO`가 NAS 공유폴더 `photo`와 충돌해 `/Volumes/PHOTO 1`로
//! 마운트된 사례가 있었다. 절대경로를 DB에 넣으면 그날로 라이브러리가 깨진다.
//!
//! UUID는 `getattrlist(ATTR_VOL_UUID)`로 얻는다. `diskutil info`가 보여주는
//! "Volume UUID"와 같은 값이고, 외부 프로세스를 띄우지 않는다.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_VOL_INFO: u32 = 0x8000_0000;
const ATTR_VOL_UUID: u32 = 0x0004_0000;

/// 마운트된 볼륨 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `CD726B15-E2D4-323C-802A-DF8E575E8A44` 형태. 이것이 영구 식별자다.
    pub uuid: String,
    /// 표시용 이름. 사용자가 바꿀 수 있으므로 식별에 쓰지 않는다.
    pub name: String,
    /// 지금 이 순간의 마운트 지점. 다음에 달라질 수 있다.
    pub mount_path: PathBuf,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error("경로에 0 바이트가 들어 있습니다")]
    BadPath,
    #[error("볼륨 정보를 읽을 수 없습니다: {0}")]
    Io(#[from] std::io::Error),
    #[error("이 볼륨은 UUID를 제공하지 않습니다 (네트워크 공유일 수 있습니다)")]
    NoUuid,
}

type Result<T> = std::result::Result<T, VolumeError>;

/// 경로가 속한 볼륨의 UUID를 읽는다.
///
/// SMB/AFP 같은 네트워크 마운트는 UUID를 주지 않는 경우가 있다. 그때는
/// [`VolumeError::NoUuid`]가 돌아오므로 호출 쪽에서 판단해야 한다 — NAS는
/// 애초에 라이브러리 볼륨으로 등록하지 않는다.
pub fn volume_uuid(path: impl AsRef<Path>) -> Result<String> {
    let c = CString::new(path.as_ref().as_os_str().as_bytes()).map_err(|_| VolumeError::BadPath)?;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    // getattrlist는 길이 4바이트를 앞에 붙여 돌려준다.
    #[repr(C)]
    struct UuidBuf {
        length: u32,
        uuid: [u8; 16],
    }

    let mut al = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: ATTR_VOL_INFO | ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buf = UuidBuf {
        length: 0,
        uuid: [0u8; 16],
    };

    // SAFETY: c는 유효한 NUL 종료 문자열, al/buf는 스택에 있고 크기를 정확히 넘긴다.
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            &mut al as *mut _ as *mut libc::c_void,
            &mut buf as *mut _ as *mut libc::c_void,
            std::mem::size_of::<UuidBuf>(),
            0,
        )
    };
    if rc != 0 {
        return Err(VolumeError::Io(std::io::Error::last_os_error()));
    }
    // 값이 안 왔거나 전부 0이면 UUID를 제공하지 않는 파일시스템이다.
    if (buf.length as usize) < std::mem::size_of::<UuidBuf>() || buf.uuid.iter().all(|&b| b == 0) {
        return Err(VolumeError::NoUuid);
    }
    Ok(format_uuid(&buf.uuid))
}

/// 16바이트를 `8-4-4-4-12` 대문자 형식으로 (diskutil과 같은 표기).
fn format_uuid(b: &[u8; 16]) -> String {
    let hex = |r: &[u8]| r.iter().map(|x| format!("{x:02X}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        hex(&b[0..4]),
        hex(&b[4..6]),
        hex(&b[6..8]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

/// 경로가 속한 볼륨의 마운트 지점과 용량.
pub fn volume_stat(path: impl AsRef<Path>) -> Result<(PathBuf, u64, u64)> {
    let c = CString::new(path.as_ref().as_os_str().as_bytes()).map_err(|_| VolumeError::BadPath)?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c는 유효한 경로, st는 스택에 있다.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(VolumeError::Io(std::io::Error::last_os_error()));
    }
    let mount = unsafe { std::ffi::CStr::from_ptr(st.f_mntonname.as_ptr()) };
    let mount = normalize_mount(PathBuf::from(std::ffi::OsStr::from_bytes(mount.to_bytes())));
    let bsize = st.f_bsize as u64;
    Ok((mount, st.f_blocks * bsize, st.f_bavail * bsize))
}

/// macOS APFS는 Catalina부터 시스템 볼륨과 데이터 볼륨을 나눈다. 사용자 데이터의
/// 실제 마운트 지점은 `/System/Volumes/Data`인데, 우리가 다루는 경로는 firmlink를
/// 거친 `/Users/...`나 `/var/...`다. 그대로 두면 `strip_prefix`가 항상 실패한다.
fn normalize_mount(p: PathBuf) -> PathBuf {
    if p == Path::new("/System/Volumes/Data") {
        PathBuf::from("/")
    } else {
        p
    }
}

/// 경로 하나로 볼륨 정보를 모아 온다.
pub fn describe(path: impl AsRef<Path>) -> Result<VolumeInfo> {
    let path = path.as_ref();
    let uuid = volume_uuid(path)?;
    let (mount_path, total_bytes, free_bytes) = volume_stat(path)?;
    let name = mount_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        // 부팅 볼륨은 마운트 지점이 "/" 라 이름이 없다.
        .unwrap_or_else(|| "Macintosh HD".to_string());
    Ok(VolumeInfo {
        uuid,
        name,
        mount_path,
        total_bytes,
        free_bytes,
    })
}

/// 저장해 둔 UUID의 볼륨이 지금 어디에 붙어 있는지 찾는다.
///
/// 이것이 이 모듈의 존재 이유다. 라이브러리를 열 때 UUID로 실제 위치를 다시
/// 찾으면, 마운트 경로가 바뀌었어도 파일을 잃지 않는다.
pub fn find_mount(uuid: &str) -> Option<PathBuf> {
    // 부팅 볼륨을 먼저 본다. macOS는 이 볼륨을 "/" 와 "/Volumes/<이름>" 양쪽으로
    // 노출하는데, 정규 경로는 "/" 다.
    //
    // "/"와 "/System/Volumes/Data"는 **UUID가 서로 다르다** (APFS 시스템/데이터
    // 분리). 사용자 폴더는 전부 Data 볼륨에 있으므로 둘 다 확인해야 한다.
    // 하나만 봤을 때는 부팅 디스크 안의 라이브러리가 아래 스냅숏에 잡혔다.
    for p in ["/", "/System/Volumes/Data"] {
        if volume_uuid(p).ok().as_deref() == Some(uuid) {
            return Some(PathBuf::from("/"));
        }
    }
    for entry in std::fs::read_dir("/Volumes").ok()?.flatten() {
        let p = entry.path();
        // 타임머신 로컬 스냅숏은 원본 볼륨과 UUID가 같다. 여기로 경로를 풀면
        // **읽기 전용 스냅숏의 옛 내용**을 보게 된다.
        if p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("com.apple.TimeMachine"))
        {
            continue;
        }
        if volume_uuid(&p).ok().as_deref() == Some(uuid) {
            return Some(p);
        }
    }
    None
}

/// 볼륨 내 상대경로로 바꾼다. 볼륨 밖 경로면 None.
pub fn to_relative(mount: &Path, full: &Path) -> Option<PathBuf> {
    full.strip_prefix(mount).ok().map(|p| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_candidate_volume(name: &str) -> bool {
        !name.starts_with('.') && !name.starts_with("com.apple.")
    }

    /// 임시 폴더의 볼륨을 UUID로 되찾을 수 있어야 한다.
    ///
    /// 사용자 폴더는 Data 볼륨에 있는데 "/"는 System 볼륨이라 UUID가 다르다.
    /// 이걸 놓치면 타임머신 스냅숏이 대신 잡혀 옛 내용을 읽는다.
    #[test]
    fn a_temp_dir_resolves_back_to_the_boot_volume() {
        let dir = tempfile::tempdir().unwrap();
        let v = describe(dir.path()).expect("볼륨 인식");
        assert_eq!(
            find_mount(&v.uuid).as_deref(),
            Some(Path::new("/")),
            "부팅 볼륨은 \"/\"로 돌아와야 한다 (UUID {})",
            v.uuid
        );
        // 되찾은 마운트 + 상대경로 = 원래 경로
        let rel = to_relative(&v.mount_path, dir.path()).expect("상대경로");
        assert!(find_mount(&v.uuid).unwrap().join(&rel).is_dir());
    }

    #[test]
    fn boot_volume_has_a_uuid() {
        let uuid = volume_uuid("/").expect("부팅 볼륨은 UUID가 있어야 한다");
        assert_eq!(uuid.len(), 36, "8-4-4-4-12 형식: {uuid}");
        assert_eq!(uuid.matches('-').count(), 4);
        assert!(
            uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "16진수와 하이픈만: {uuid}"
        );
    }

    #[test]
    fn format_matches_diskutil_style() {
        let b: [u8; 16] = [
            0xCD, 0x72, 0x6B, 0x15, 0xE2, 0xD4, 0x32, 0x3C, 0x80, 0x2A, 0xDF, 0x8E, 0x57, 0x5E,
            0x8A, 0x44,
        ];
        assert_eq!(format_uuid(&b), "CD726B15-E2D4-323C-802A-DF8E575E8A44");
    }

    #[test]
    fn describe_reports_capacity() {
        let v = describe("/").expect("부팅 볼륨");
        assert!(v.total_bytes > 0, "총 용량이 0일 수 없다");
        assert!(v.free_bytes <= v.total_bytes);
        assert_eq!(v.mount_path, PathBuf::from("/"));
    }

    #[test]
    fn find_mount_round_trips() {
        let v = describe("/").expect("부팅 볼륨");
        let found = find_mount(&v.uuid).expect("UUID로 다시 찾을 수 있어야 한다");
        assert_eq!(found, v.mount_path);
    }

    #[test]
    fn unknown_uuid_is_not_found() {
        assert!(find_mount("00000000-0000-0000-0000-000000000000").is_none());
    }

    #[test]
    fn data_volume_is_reported_as_root() {
        // APFS 데이터 볼륨을 그대로 두면 사용자 경로에서 strip_prefix가 실패한다
        assert_eq!(
            normalize_mount(PathBuf::from("/System/Volumes/Data")),
            PathBuf::from("/")
        );
        // 외장 볼륨은 그대로 둔다
        assert_eq!(
            normalize_mount(PathBuf::from("/Volumes/PHOTO 1")),
            PathBuf::from("/Volumes/PHOTO 1")
        );
    }

    #[test]
    fn user_paths_are_relative_to_the_boot_volume() {
        // 실제로 홈 디렉터리가 볼륨 상대경로로 바뀌어야 한다
        let home = std::env::var("HOME").unwrap();
        let v = describe(&home).expect("홈 볼륨");
        let rel = to_relative(&v.mount_path, Path::new(&home));
        assert!(rel.is_some(), "홈 경로가 볼륨 상대경로로 바뀌어야 한다");
        assert!(!rel.unwrap().to_string_lossy().starts_with('/'));
    }

    #[test]
    fn relative_path_conversion() {
        let mount = Path::new("/Volumes/PHOTO 1");
        let full = Path::new("/Volumes/PHOTO 1/2018/a.jpg");
        assert_eq!(to_relative(mount, full), Some(PathBuf::from("2018/a.jpg")));
        // 다른 볼륨의 경로는 걸러진다
        assert_eq!(to_relative(mount, Path::new("/Users/x/a.jpg")), None);
    }

    /// 실제 외장 볼륨이 붙어 있을 때만 도는 테스트.
    /// 숨김 항목, `com.apple.` 항목, 심볼릭 링크, UUID 없는 마운트는 건너뛰고 UUID가 안정적인지 본다.
    #[test]
    fn external_volume_uuid_is_stable() {
        let Some(entries) = std::fs::read_dir("/Volumes").ok() else {
            return;
        };

        for entry in entries.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !is_candidate_volume(name) {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if metadata.is_symlink() {
                continue;
            }
            let Ok(uuid) = volume_uuid(&p) else {
                continue; // 네트워크 마운트 등
            };
            let again = find_mount(&uuid).expect("UUID로 되찾을 수 있어야 한다");
            assert_eq!(
                volume_uuid(&again).unwrap(),
                uuid,
                "같은 볼륨을 가리켜야 한다"
            );
        }
    }

    #[test]
    fn candidate_volume_name_excludes_hidden_and_apple_entries() {
        assert!(is_candidate_volume("PHOTO 1"));
        assert!(!is_candidate_volume(".hidden"));
        assert!(!is_candidate_volume("com.apple.TimeMachine.localsnapshots"));
    }

    #[test]
    fn missing_path_is_an_error() {
        assert!(volume_uuid("/nonexistent-path-for-test").is_err());
    }
}
