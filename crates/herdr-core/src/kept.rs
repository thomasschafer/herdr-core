//! Lost kept records stay live without a holder until a replacement shell starts.
use crate::config::ProgramName;
use crate::foreground::Foreground;
use crate::holder_proto as proto;
use crate::input::TypedLine;
use crate::runtime::{Core, now_ms};
use crate::terminals::{KEEP_RETRY_MS, Terminal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Terminal {
    pub(crate) fn schedule_restart(&mut self, now: u64) {
        let stable = self.program_started_ms.is_some_and(|at| now.saturating_sub(at) >= 10_000);
        self.restart_delay_ms = if stable {
            KEEP_RETRY_MS
        } else {
            match self.restart_delay_ms {
                0 => KEEP_RETRY_MS,
                KEEP_RETRY_MS => 3000,
                3000 => 6000,
                6000 => 12_000,
                12_000 => 30_000,
                _ => 60_000,
            }
        };
        self.restart_after_ms = now + self.restart_delay_ms;
        self.program_started_ms = None;
    }
}

/// What to type into the replacement shell, started in `cwd`, of a terminal
/// whose program was lost while `lost` led its foreground job: None unless
/// `allowed` names it (as the OS names it, or by its argv[0]); an error when
/// it can't be typed back as it ran. It is started again through PATH, by
/// the allowed name, with its own arguments.
pub(crate) fn restore_line(lost: &Foreground, allowed: &[ProgramName], cwd: &str) -> Option<Result<TypedLine, String>> {
    let argv0 = lost.argv.first().and_then(|a| std::path::Path::new(a).file_name()).and_then(|n| n.to_str());
    let name = allowed.iter().find(|c| c.as_str() == lost.name || Some(c.as_str()) == argv0)?;
    let line = || {
        if lost.argv.is_empty() {
            return Err("its arguments could not be read".to_string());
        }
        if lost.cwd != cwd {
            return Err(format!("it ran in {}, but the shell starts in {cwd}", lost.cwd));
        }
        command_line(name, &lost.argv[1..])
    };
    Some(line())
}

/// `program` and its `args` as one line that this machine's shell reads back
/// as them; an error naming what can't be typed.
pub(crate) fn command_line(program: &ProgramName, args: &[String]) -> Result<TypedLine, String> {
    let mut line = program.as_str().to_string();
    for arg in args {
        let quoted = Some(arg)
            .filter(|a| !a.chars().any(char::is_control))
            .and_then(|a| crate::platform::quote_arg(a))
            .ok_or_else(|| format!("its argument {arg:?} cannot be typed into a shell"))?;
        line = format!("{line} {quoted}");
    }
    TypedLine::try_from(line).map_err(|why| format!("its command line can't be typed: {why}"))
}

/// How a terminal's foreground program said it starts again after a loss
/// (`terminal.resume`, e.g. an agent with the session to reopen): typed into
/// the replacement shell, which starts where the program ran. It is
/// forgotten once another job has led the terminal for a while (see
/// `RESUME_GRACE_MS`).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Resume {
    /// What is typed, then Enter.
    pub line: TypedLine,
    /// The program's folder when it said so.
    pub cwd: String,
    /// The leader of the foreground job that said so.
    pub pid: i32,
    /// When another job took the lead, ms since the epoch. Never saved, so a
    /// loss before the grace is up still resumes.
    #[serde(skip)]
    pub ended_ms: Option<u64>,
}

/// How long another job leads before a resume is forgotten. A reboot or
/// logout can end the program a moment before the core, handing the
/// foreground to its shell: that must not count as the program having quit.
pub const RESUME_GRACE_MS: u64 = 10_000;

fn key(r: &Resume) -> (String, String) {
    (r.cwd.clone(), r.line.as_str().to_string())
}

/// `resume`, if it can be typed into a recovered terminal: its folder is
/// still there, and no other terminal runs or is resuming the same line in
/// the same folder (two terminals must not reopen one agent session).
fn claim(
    resume: Resume,
    taken: &mut HashSet<(String, String)>,
    is_dir: impl Fn(&str) -> bool,
) -> Result<Resume, String> {
    if !is_dir(&resume.cwd) {
        return Err(format!("its folder {} is gone", resume.cwd));
    }
    match taken.insert(key(&resume)) {
        true => Ok(resume),
        false => Err(format!("another terminal already runs {:?} in {}", resume.line.as_str(), resume.cwd)),
    }
}

/// A saved resume this core can no longer type is dropped, loudly, rather
/// than making the whole record unreadable.
pub(crate) fn lenient_resume<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Resume>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value.and_then(|v| serde_json::from_value(v).map_err(|e| eprintln!("core: dropping a saved resume: {e}")).ok()))
}

impl Core {
    pub(crate) fn spawn_failed(&self, st: &mut crate::runtime::State, id: u32) {
        let t = st.reg.terminals.get_mut(&id).expect("failed spawn");
        t.holder = None;
        t.pid = None;
        if st.killing.contains(&id) || !t.keep || t.reason.as_deref().is_none_or(|r| r == "spawn_pending") {
            let reason = t.reason.clone().filter(|r| r != "spawn_pending").unwrap_or_else(|| "holder_lost".into());
            self.record_finish(st, id, proto::Exit { code: None, signal: None }, &reason);
        } else {
            t.schedule_restart(now_ms());
        }
        self.save(st);
        self.changed.notify_waiters();
    }

    pub(crate) async fn recover_kept(self: Arc<Self>, startup: bool) {
        let pending = self.lock().reg.terminals.values().any(|t| t.is_live() && t.keep && t.holder.is_none());
        if !pending || crate::platform::host_shutdown_in_progress() {
            return;
        }
        if self.sweeps.load(Ordering::SeqCst) != 0 || self.lock().stopping || self.restart.load(Ordering::SeqCst) {
            return;
        }
        if let Err(e) = self.ensure_holder().await {
            eprintln!("core: kept terminals waiting for a holder: {e}");
            return;
        }
        let base_env = crate::platform::program_env().await;
        let allowed = crate::config::load(&self.paths.config).map(|c| c.restore_commands).unwrap_or_else(|e| {
            eprintln!("core: restoring no programs: {}", e.message);
            Vec::new()
        });
        let mut st = self.lock();
        if st.stopping
            || self.restart.load(Ordering::SeqCst)
            || self.sweeps.load(Ordering::SeqCst) != 0
            || crate::platform::host_shutdown_in_progress()
        {
            return;
        }
        let ids: Vec<u32> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.is_live() && t.keep && t.holder.is_none() && (startup || t.restart_after_ms <= now_ms()))
            .map(|t| t.id())
            .collect();
        let mut taken: HashSet<(String, String)> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.holder.is_some())
            .filter_map(|t| t.resume.as_ref())
            .map(key)
            .collect();
        for id in ids {
            let Some(holder) = st.current() else { break };
            let instance = holder.instance.clone();
            let t = st.reg.terminals.get_mut(&id).expect("pending");
            let env = crate::api::child_env(base_env.clone(), t.restart_env.clone());
            // Kept until the new shell's own foreground replaces it, so a failed spawn's retry still resumes.
            let resume = t.resume.clone().and_then(|r| {
                claim(r, &mut taken, |p| std::path::Path::new(p).is_dir())
                    .map_err(|why| eprintln!("core: {} is not resumed: {why}", t.address))
                    .ok()
            });
            let cwd = resume
                .as_ref()
                .map(|r| &r.cwd)
                .or_else(|| t.last_cwd.as_ref().filter(|p| std::path::Path::new(p).is_dir()))
                .or_else(|| std::path::Path::new(&t.cwd).is_dir().then_some(&t.cwd))
                .cloned()
                .unwrap_or_else(|| {
                    env.get("HOME").or_else(|| env.get("USERPROFILE")).cloned().unwrap_or_else(|| "/".into())
                });
            let typed = match resume {
                Some(r) => Some(r.line),
                None => match t.lost_foreground.as_ref().and_then(|lost| restore_line(lost, &allowed, &cwd)) {
                    Some(Err(why)) => {
                        eprintln!("core: {} gets a plain shell: {why}", t.address);
                        None
                    }
                    typed => typed.and_then(Result::ok),
                },
            };
            let (env, scope) = crate::api::terminal_env(&self, id, &cwd, env);
            t.scope = Some(scope);
            t.holder = Some(instance);
            t.argv = crate::platform::own_shell();
            t.cwd = cwd.clone();
            t.template = None;
            t.reported = false;
            t.title.clear();
            t.full_screen = false;
            t.win32_input = false;
            t.printed = 0;
            t.screen = "exact".into();
            t.submit = None;
            t.stop = None;
            t.paste_gap_ms = None;
            let spawn = proto::Spawn { argv: t.argv.clone(), env, cwd, cols: t.cols, rows: t.rows };
            let mut screen = ghostty_vt::Terminal::new(t.cols, t.rows, self.history_bytes());
            screen.set_xtversion(&t.identity);
            st.screens.insert(id, screen);
            // Claim and persist before sending: a slow holder never gets a second spawn.
            self.save(&st);
            if let Some(link) = st.link(id) {
                link.send(&proto::json_frame(proto::SPAWN, id, &spawn));
                // The holder starts the shell before it reads the next frame, so this reaches the new shell.
                if let Some(line) = typed {
                    link.input(id, &line.with_enter());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::restore_line;
    use crate::foreground::Foreground;
    use crate::terminals::{NewTerminal, Registry};

    fn lost(name: &str, argv: &[&str]) -> Foreground {
        Foreground { pid: 9, name: name.into(), argv: argv.iter().map(|a| a.to_string()).collect(), cwd: "/r".into() }
    }

    fn allowed(names: &[&str]) -> Vec<crate::config::ProgramName> {
        names.iter().map(|n| n.to_string().try_into().unwrap()).collect()
    }

    #[test]
    fn only_allowed_programs_are_typed_again_through_path_by_their_allowed_name() {
        let hx = allowed(&["hx"]);
        let line = |f: &Foreground| restore_line(f, &hx, "/r").map(|l| l.unwrap().as_str().to_string());
        let typed = line(&lost("hx", &["/opt/bin/hx", "src/main.rs", "my notes.txt"]));
        assert_eq!(typed.as_deref(), Some("hx 'src/main.rs' 'my notes.txt'"));
        assert_eq!(line(&lost("helix-term", &["hx"])).as_deref(), Some("hx"), "argv[0] names it too");
        assert_eq!(line(&lost("vim", &["vim", "a"])), None);
        assert_eq!(line(&lost("zsh", &["-zsh"])), None);
        assert_eq!(restore_line(&lost("hx", &["hx"]), &[], "/r"), None, "an empty list restores nothing");
    }

    #[test]
    fn an_allowed_program_that_cannot_be_typed_back_as_it_ran_is_refused() {
        let hx = allowed(&["hx"]);
        let refused = |f: Foreground, cwd: &str| restore_line(&f, &hx, cwd).expect("allowed").unwrap_err();
        assert!(refused(lost("hx", &[]), "/r").contains("arguments could not be read"));
        assert!(refused(lost("hx", &["hx"]), "/home").contains("ran in /r, but the shell starts in /home"));
        for arg in ["it's", "a\nb"] {
            assert!(refused(lost("hx", &["hx", arg]), "/r").contains("cannot be typed"), "{arg:?}");
        }
        let long = "x".repeat(crate::input::MAX_TYPED);
        assert!(refused(lost("hx", &["hx", &long]), "/r").contains("over 1024 bytes"));
    }

    fn resume(cwd: &str, line: &str) -> super::Resume {
        super::Resume { line: line.parse().unwrap(), cwd: cwd.into(), pid: 9, ended_ms: None }
    }

    #[test]
    fn a_resume_is_typed_once_and_only_into_its_own_folder() {
        let mut taken = std::collections::HashSet::new();
        let is_dir = |p: &str| p != "/gone";
        let mut claim = |r| super::claim(r, &mut taken, is_dir);
        assert!(claim(resume("/r", "claude '--resume' 'a'")).is_ok());
        let twice = claim(resume("/r", "claude '--resume' 'a'")).unwrap_err();
        assert!(twice.contains("another terminal"), "{twice}");
        assert!(claim(resume("/s", "claude '--resume' 'a'")).is_ok(), "another folder is another session");
        assert!(claim(resume("/r", "claude '--resume' 'b'")).is_ok());
        assert!(claim(resume("/gone", "codex 'resume' 'c'")).unwrap_err().contains("folder /gone is gone"));
    }

    #[test]
    fn a_saved_resume_this_core_cannot_type_is_dropped_with_the_rest_of_the_record_kept() {
        let mut reg = Registry::default();
        let id = reg.create(NewTerminal { argv: vec!["sh".into()], keep: true, ..Default::default() }, 0).unwrap();
        let t = reg.terminals.get_mut(&id).unwrap();
        t.resume = Some(resume("/r", "codex 'resume' 'c'"));
        let mut saved = serde_json::to_value(&*t).unwrap();
        let back: crate::terminals::Terminal = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(back.resume, t.resume);
        saved["resume"]["line"] = "x".repeat(crate::input::MAX_TYPED).into();
        let back: crate::terminals::Terminal = serde_json::from_value(saved).unwrap();
        assert_eq!((back.resume, back.keep), (None, true));
    }

    #[test]
    fn retries_back_off_forever_and_reset_only_after_a_stable_program() {
        let mut reg = Registry::default();
        let id = reg.create(NewTerminal { argv: vec!["sh".into()], keep: true, ..Default::default() }, 0).unwrap();
        let t = reg.terminals.get_mut(&id).unwrap();
        for delay in [1500, 3000, 6000, 12_000, 30_000, 60_000, 60_000] {
            t.schedule_restart(100_000);
            assert_eq!(t.restart_delay_ms, delay);
            assert_eq!(t.restart_after_ms, 100_000 + delay);
        }
        t.program_started_ms = Some(90_001);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 60_000, "a short-lived program does not reset retries");
        t.program_started_ms = Some(90_000);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 1500);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 3000);
        assert!(t.is_live());
    }
}
