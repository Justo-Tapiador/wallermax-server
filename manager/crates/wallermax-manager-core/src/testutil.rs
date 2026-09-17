//! Unit-battery helpers: unique temporary directories, bounded polling and
//! the cross-platform command specifications the process tests spawn.
//!
//! The Windows specifications spawn the well-known system tools
//! *directly* — `ping.exe` and `cmd.exe` live in `System32`, which
//! `CreateProcess` always searches, so the commands resolve even under a
//! stripped `PATH`. No command is ever wrapped in a shell by the manager
//! itself; where a compound behaviour is needed the test names the shell
//! (`cmd` / `sh`) as the program explicitly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::process_manager::SpawnSpec;

/// Creates a unique temporary directory for one test.
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "wallermax-manager-test-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// Polls `probe` every 30 ms until it answers `true`, panicking after ten
/// seconds with `what` as the explanation. Bounded waiting keeps the unit
/// batteries honest: a hang fails loudly instead of freezing the suite.
pub(crate) fn wait_for(what: &str, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if probe() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// A process that prints a line and then stays alive for half a minute:
/// `ping -n 30 127.0.0.1` on Windows, `echo` + `sleep` on Unix. Spawned
/// directly (never through a shell wrapper), so the process the manager
/// tracks *is* the process that owns the lifetime.
pub(crate) fn survivor_spec() -> SpawnSpec {
    if cfg!(windows) {
        SpawnSpec {
            program: "cmd".to_owned(),
            args: vec![
                "/C".into(),
                "echo".into(),
                "manager-spawn-ok".into(),
                "&".into(),
                "ping".into(),
                "-n".into(),
                "30".into(),
                "127.0.0.1".into(),
            ],
            cwd: std::env::temp_dir(),
        }
    } else {
        SpawnSpec {
            program: "sh".to_owned(),
            args: vec!["-c".into(), "echo manager-spawn-ok; sleep 30".into()],
            cwd: std::env::temp_dir(),
        }
    }
}

/// A process that prints a reason on stderr and exits with code 3: the
/// shape of a server refusing to boot.
pub(crate) fn refusal_spec() -> SpawnSpec {
    if cfg!(windows) {
        SpawnSpec {
            program: "cmd".to_owned(),
            args: vec![
                "/C".into(),
                "echo".into(),
                "manager-refusal-reason".into(),
                "1>&2".into(),
                "&".into(),
                "exit".into(),
                "/b".into(),
                "3".into(),
            ],
            cwd: std::env::temp_dir(),
        }
    } else {
        SpawnSpec {
            program: "sh".to_owned(),
            args: vec![
                "-c".into(),
                "echo manager-refusal-reason 1>&2; exit 3".into(),
            ],
            cwd: std::env::temp_dir(),
        }
    }
}

/// A process that prints `first`, pauses about three seconds, prints
/// `second`, and then stays alive — the timeline the sequence-cursor test
/// walks. The pause outlasts the boot-probe window, so the test always
/// captures its first page between the two lines.
pub(crate) fn chatty_spec() -> SpawnSpec {
    if cfg!(windows) {
        SpawnSpec {
            program: "cmd".to_owned(),
            args: vec![
                "/C".into(),
                "echo".into(),
                "first".into(),
                "&".into(),
                "ping".into(),
                "-n".into(),
                "4".into(),
                "127.0.0.1".into(),
                ">nul".into(),
                "&".into(),
                "echo".into(),
                "second".into(),
                "&".into(),
                "ping".into(),
                "-n".into(),
                "30".into(),
                "127.0.0.1".into(),
            ],
            cwd: std::env::temp_dir(),
        }
    } else {
        SpawnSpec {
            program: "sh".to_owned(),
            args: vec![
                "-c".into(),
                "echo first; sleep 3; echo second; sleep 30".into(),
            ],
            cwd: std::env::temp_dir(),
        }
    }
}

/// A process that exits successfully within a moment.
pub(crate) fn short_lived_spec() -> SpawnSpec {
    if cfg!(windows) {
        SpawnSpec {
            program: "cmd".to_owned(),
            args: vec!["/C".into(), "exit".into(), "/b".into(), "0".into()],
            cwd: std::env::temp_dir(),
        }
    } else {
        SpawnSpec {
            program: "sh".to_owned(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: std::env::temp_dir(),
        }
    }
}

/// The survivor as a *command line string*, the way the settings store it.
pub(crate) fn survivor_command_line() -> &'static str {
    if cfg!(windows) {
        "cmd /C echo app-server-up & ping -n 30 127.0.0.1"
    } else {
        "sh -c \"echo app-server-up; sleep 30\""
    }
}
