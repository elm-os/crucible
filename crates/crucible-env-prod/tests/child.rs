//! The production `ChildProcess` against real processes. The shell scripts need a Unix host; the Windows
//! behaviour is covered by the proxy's process tests, which run a helper binary.
#![cfg(unix)]

use std::time::Duration;

use crucible_env::{
    ChildError, ChildProcess, ChildSignal, ChildSpec, Clock, ClockExt, ExitStatus, StderrMode,
};
use crucible_env_prod::{SystemClock, TokioChildProcess};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn sh(script: &str) -> ChildSpec {
    ChildSpec {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        ..ChildSpec::default()
    }
}

#[tokio::test]
async fn pipes_carry_bytes_and_the_exit_code_comes_back() {
    let mut child = TokioChildProcess::new()
        .spawn(ChildSpec {
            env: vec![("CRUCIBLE_TEST_VALUE".to_owned(), "seven".to_owned())],
            stderr: StderrMode::Pipe,
            ..sh("cat; echo \"$CRUCIBLE_TEST_VALUE\" >&2; exit 7")
        })
        .await
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"hello\n").await.unwrap();
    drop(stdin);
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .await
        .unwrap();
    assert_eq!(out, "hello\n");
    assert_eq!(err, "seven\n");
    assert_eq!(child.wait().await.unwrap(), ExitStatus::Code(7));
    // The status is remembered.
    assert_eq!(child.wait().await.unwrap(), ExitStatus::Code(7));
}

#[tokio::test]
async fn a_forwarded_signal_ends_the_child_with_that_signal() {
    let mut child = TokioChildProcess::new()
        .spawn(sh("echo ready; sleep 30"))
        .await
        .unwrap();
    let mut ready = [0u8; 6];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .await
        .unwrap();
    child.signal(ChildSignal::Terminate).unwrap();
    assert_eq!(child.wait().await.unwrap(), ExitStatus::Signal(15));
}

#[tokio::test]
async fn wait_does_not_wait_for_a_lingering_grandchild_and_drop_ends_it() {
    // The grandchild keeps running after the child exits and holds no pipe of ours.
    let mut child = TokioChildProcess::new()
        .spawn(sh("sleep 300 >/dev/null 2>&1 </dev/null & echo $!; exit 3"))
        .await
        .unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    let grandchild = out.trim().to_owned();
    let clock = SystemClock::new();
    let status = clock
        .timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("wait returned without the grandchild")
        .unwrap();
    assert_eq!(status, ExitStatus::Code(3));
    let alive = |pid: &str| std::path::Path::new(&format!("/proc/{pid}")).exists();
    if cfg!(target_os = "linux") {
        assert!(alive(&grandchild), "the grandchild outlived the child");
    }
    drop(child);
    if cfg!(target_os = "linux") {
        for _ in 0..200 {
            if !alive(&grandchild) {
                return;
            }
            clock.sleep(Duration::from_millis(10)).await;
        }
        panic!("the grandchild survived the drop of its group");
    }
}

#[tokio::test]
async fn an_unknown_program_is_refused_with_a_hint() {
    let error = TokioChildProcess::new()
        .spawn(ChildSpec {
            program: "crucible-no-such-program".to_owned(),
            ..ChildSpec::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, ChildError::NotFound(_)), "{error:?}");
    assert!(error.to_string().contains("add its directory to PATH"));
}
