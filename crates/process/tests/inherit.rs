#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use ghostrealm_process::inherit_stdio_only;

fn long_lived() -> Child {
    let mut cmd = Command::new("sleep");
    cmd.arg("30").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    inherit_stdio_only(&mut cmd).spawn().unwrap()
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Given threads starting long-lived children while others make pipes as
/// `Command::output()` does and wait for their end, and others call
/// `output()` itself;
/// when they race for a second;
/// then every wait returns at once rather than when a child exits.
///
/// A pipe is inheritable only between two syscalls, so bare pipes, made far
/// faster than `output()` can spawn, are what make the race likely.
#[test]
fn pipes_end_while_long_lived_children_start_alongside() {
    const PIPERS: usize = 4;
    const CALLERS: usize = 2;
    let stop_spawning = Arc::new(AtomicBool::new(false));
    let spawners: Vec<_> = (0..2)
        .map(|_| {
            let stopping = stop_spawning.clone();
            std::thread::spawn(move || {
                let mut children = Vec::new();
                while !stopping.load(Ordering::Relaxed) && children.len() < 100 {
                    children.push(long_lived());
                }
                children
            })
        })
        .collect();
    let (tx, rx) = mpsc::channel();
    let race = |wait: fn()| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(1);
            let mut slowest = Duration::ZERO;
            while Instant::now() < until {
                let start = Instant::now();
                wait();
                slowest = slowest.max(start.elapsed());
            }
            let _ = tx.send(slowest);
        });
    };
    for _ in 0..PIPERS {
        race(|| {
            let (mut reader, writer) = std::io::pipe().unwrap();
            drop(writer);
            assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        });
    }
    for _ in 0..CALLERS {
        race(|| assert!(Command::new("true").output().unwrap().status.success()));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let slowest: Vec<_> = (0..PIPERS + CALLERS)
        .map_while(|_| rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok())
        .collect();
    stop_spawning.store(true, Ordering::Relaxed);
    // Killing the children ends any wait still held up by them.
    for spawner in spawners {
        for child in spawner.join().unwrap() {
            stop(child);
        }
    }
    assert_eq!(slowest.len(), PIPERS + CALLERS, "a pipe stayed open in a long-lived child");
    let slowest = slowest.into_iter().max().unwrap();
    assert!(slowest < Duration::from_secs(2), "a wait took {slowest:?}");
}

/// A program that doesn't exist is still reported as a spawn error: std's
/// exec-error pipe stays open until the exec.
#[test]
fn a_failed_exec_is_still_reported() {
    let err = inherit_stdio_only(&mut Command::new("/nonexistent/ghostrealm-test")).spawn().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// The child's own piped stdin and stdout still work.
#[test]
fn piped_stdio_still_works() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "read line; echo \"got $line\""]).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = inherit_stdio_only(&mut cmd).spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "hello").unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line, "got hello\n");
    assert!(child.wait().unwrap().success());
}
