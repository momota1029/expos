#[cfg(windows)]
use std::{
    env,
    io::{Read, Write},
    path::Path,
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use sha2::{Digest, Sha256};

#[cfg(windows)]
const VERSION: u16 = 1;

#[cfg(windows)]
fn read_request() -> Vec<u8> {
    let mut stdin = std::io::stdin().lock();
    let mut length = [0; 4];
    stdin.read_exact(&mut length).expect("read request length");
    let mut frame = vec![0; u32::from_le_bytes(length) as usize];
    stdin.read_exact(&mut frame).expect("read request frame");
    frame
}

#[cfg(windows)]
fn write_frame(frame: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&(frame.len() as u32).to_le_bytes())
        .and_then(|_| stdout.write_all(frame))
        .and_then(|_| stdout.flush())
        .expect("write framed helper response");
}

#[cfg(windows)]
fn wait_for_release() {
    let path = env::var_os("GPUI_FILER_ECHO_RELEASE")
        .map(std::path::PathBuf::from)
        .expect("release marker path");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "helper release timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(windows)]
fn hold() {
    let request = read_request();
    assert_eq!(&request[..4], b"EIPC");
    assert_eq!(request[6], 2, "expected a Search request");
    let ready = env::var("GPUI_FILER_HELPER_READY").expect("ready marker path");
    std::fs::write(ready, std::process::id().to_string()).expect("write helper pid");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(windows)]
fn append_wide_path(output: &mut Vec<u8>, path: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    let units = path.encode_wide().collect::<Vec<_>>();
    output.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        output.extend_from_slice(&unit.to_le_bytes());
    }
}

#[cfg(windows)]
fn echo() {
    let request = read_request();
    assert_eq!(&request[..4], b"EIPC");
    assert_eq!(
        u16::from_le_bytes(request[4..6].try_into().unwrap()),
        VERSION
    );
    assert_eq!(request[6], 2, "expected a Search request");
    let request_id = u64::from_le_bytes(request[7..15].try_into().unwrap());
    let revision = u64::from_le_bytes(request[15..23].try_into().unwrap());
    let query_len = u32::from_le_bytes(request[23..27].try_into().unwrap()) as usize;
    let query = std::str::from_utf8(&request[27..27 + query_len]).expect("UTF-8 query");
    assert_eq!(query, "latest-query");

    let ready = env::var("GPUI_FILER_HELPER_READY").expect("ready marker path");
    std::fs::write(ready, format!("{request_id}:{revision}"))
        .expect("write echoed request identity");

    let result_path = env::var_os("GPUI_FILER_HELPER_RESULT_PATH")
        .map(std::path::PathBuf::from)
        .expect("result path");
    let mut reply = b"EIPC".to_vec();
    reply.extend_from_slice(&VERSION.to_le_bytes());
    reply.push(2);
    reply.extend_from_slice(&request_id.to_le_bytes());
    reply.extend_from_slice(&revision.to_le_bytes());
    reply.push(0);
    reply.extend_from_slice(&1u32.to_le_bytes());
    append_wide_path(&mut reply, result_path.as_os_str());
    append_wide_path(&mut reply, Path::new("echo-result.txt").as_os_str());
    reply.push(0);
    reply.push(1);
    reply.extend_from_slice(&42u64.to_le_bytes());
    write_frame(&reply);
    wait_for_release();
}

fn main() {
    #[cfg(windows)]
    if env::args().nth(1).as_deref() == Some("--source-fingerprint") {
        println!(
            "{:x}",
            Sha256::digest(include_str!("everything_search_helper.rs").as_bytes())
        );
        return;
    }

    #[cfg(windows)]
    match env::args().nth(1).as_deref() {
        Some("hold") => hold(),
        Some("echo") => echo(),
        None => {}
        // Cargo passes test filters to harness-free targets; no fixture mode
        // means this invocation only needs to build the helper.
        Some(_) => {}
    }

    #[cfg(not(windows))]
    {}
}
