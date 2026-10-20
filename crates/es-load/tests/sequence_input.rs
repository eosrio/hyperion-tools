//! Exercise the public loader command. Invalid identities must not disappear behind a green exit
//! status and must never produce a request to the owned loopback listener.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn refuses_invalid_sequence_rows_before_posting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let path = std::env::temp_dir().join(format!(
        "hyperion-sequence-loader-{}.ndjson",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        r#"{{"block_num":42,"global_sequence":9007199254740993}}"#
    )
    .unwrap();
    writeln!(file, r#"{{"block_num":42,"global_sequence":"01"}}"#).unwrap();
    drop(file);
    let mut child = Command::new(env!("CARGO_BIN_EXE_es-load"))
        .args([
            "--file",
            path.to_str().unwrap(),
            "--es",
            &format!("http://{}", listener.local_addr().unwrap()),
            "--workers",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() >= Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            std::fs::remove_file(&path).unwrap();
            panic!("loader did not finish invalid-only input within five seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = child.wait_with_output().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("2 input document(s) were rejected")
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
