//! `hc run` works like a bash tool: it waits until the program is not
//! working (it exits, or says it is idle, done, blocked or error) and hands
//! back the output, the exit code as its own. A timeout leaves it running.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

fn hc(s: &Session, args: &[&str]) -> (i32, String) {
    let out = s.hc(args);
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn a_quick_command_returns_its_output_and_exit_code_and_leaves_no_note() {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "0".into()));
    let parent = run(&s, &["sh", "-c", &script(&s, "r state=idle; cat")]);
    let out = s.cmd().env("HC_TERMINAL_ID", &parent).args(["run", "sh", "-c", "echo hi; exit 3"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "the program's exit code");
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!text(&s, &parent).contains("<hc>"), "it saw the exit itself: no note\n{}", text(&s, &parent));

    let (code, out) = hc(&s, &["run", "echo a && echo b | tr b c"]);
    assert_eq!(code, 0);
    assert!(out.contains("a\nc"), "one quoted string is a shell line: {out:?}");
}

#[test]
fn pagers_never_hold_a_run_and_huge_output_keeps_its_tail() {
    let s = Session::new();
    let repo = s.root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let r = repo.display().to_string();
    let setup = format!(
        "cd {r} && git init -q && for i in $(seq 1 150); do git -c user.name=t -c user.email=t@t commit -q --allow-empty -m c$i; done"
    );
    assert!(std::process::Command::new("sh").args(["-c", &setup]).status().unwrap().success());
    // Longer than the 120-row screen: git would open less and wait.
    let (code, out) = hc(&s, &["run", "--timeout", "10", "--cwd", &r, "git", "log", "--oneline"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(" c1\n") || out.trim_end().ends_with(" c1"), "all of it, no pager");

    let (code, out) = hc(&s, &["run", "awk 'BEGIN{for(i=1;i<=1000;i++) printf \"%0180d\\n\", i}'"]);
    assert_eq!(code, 0);
    assert!(out.len() < 60_000, "capped: {} bytes", out.len());
    assert!(out.starts_with("[hc] earlier output cut: hc read t"), "{}", &out[..100]);
    assert!(out.trim_end().ends_with(&format!("{:0180}", 1000)), "the tail is kept");
}

#[test]
fn a_program_that_says_it_stopped_working_returns_and_a_slow_one_keeps_running() {
    let s = Session::new();
    let (code, out) = hc(
        &s,
        &["run", "sh", "-c", &script(&s, "r state=working; echo busy; sleep 0.4; r state=idle; echo ready; cat")],
    );
    assert_eq!(code, 0);
    assert!(out.contains("ready") && out.contains("idle"), "back once idle, not at working: {out:?}");

    let (code, out) = hc(&s, &["run", "--timeout", "0.5", "sh", "-c", "echo started; sleep 100"]);
    assert_eq!(code, 124, "timed out, like timeout(1)");
    assert!(out.contains("started") && out.contains("still running as t2"), "{out:?}");
    assert_eq!(get(&s, "t2")["state"], "live");

    let started = s.json(&["run", "--bg", "sleep", "100"]);
    assert_eq!((started["address"].as_str(), started["state"].as_str()), (Some("t3"), Some("live")), "--bg: at once");
    let full = s.json(&["run", "sh", "-c", "echo json"]);
    assert_eq!(
        (full["wait"]["exit_code"].as_i64(), full["output"].as_str().map(str::trim)),
        (Some(0), Some("json")),
        "{full}"
    );
}

#[test]
fn a_typed_line_runs_in_the_new_shell_at_once_and_the_shell_stays() {
    let s = Session::new();
    s.json(&["ls"]); // the core is up: what is timed is the run
    let started = std::time::Instant::now();
    let t = run(&s, &["--type", "sh -c 'echo typed-$((6*7))'", "sh"]);
    wait_until("typed line ran", || text(&s, &t).lines().any(|l| l.trim() == "typed-42"));
    if !slow_runner() {
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_millis(600), "typed ahead, not after the output settles: {took:?}");
    }
    assert_eq!(get(&s, &t)["state"], "live", "the shell outlives what it ran");
    s.json(&["send", &t, "echo after-$((1+1))"]);
    wait_until("shell still reads", || text(&s, &t).lines().any(|l| l.trim() == "after-2"));
}

#[test]
fn a_line_that_cannot_be_typed_as_plain_text_starts_nothing() {
    let s = Session::new();
    s.json(&["ls"]);
    let long = "x".repeat(1024);
    for (bad, why) in
        [("", "empty"), ("a\nb", "control character"), ("a\x1b[A", "control character"), (&long, "over 1024")]
    {
        let (code, err) = s.err(&["--json", "run", "--bg", "--type", bad, "sh"]);
        assert_ne!(code, 0);
        assert!(err.to_string().contains(why), "{bad:?}: {err}");
        let mut c = s.connect();
        let r = c.call(1, "terminal.run", serde_json::json!({"argv": ["sh"], "type": bad}));
        assert_eq!(r["error"]["data"]["kind"], "invalid_params", "{bad:?}: {r}");
        assert!(r.to_string().contains(why), "{bad:?}: {r}");
    }
    assert_eq!(s.json(&["ls", "--all"])["terminals"].as_array().map(Vec::len), Some(0), "nothing started");
}
