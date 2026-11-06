//! Recovery after a loss: whether a terminal gets a new shell, and what it types there.
use super::*;

pub(super) async fn terminal_keep(core: Ctx<'_>, p: KeepParams) -> Result<Public, RpcError> {
    let id = resolve(core, &p.terminal)?;
    let mut st = core.lock();
    let t = st.reg.terminals.get_mut(&id).expect("resolved");
    if !t.is_live() {
        return Err(RpcError::invalid_params("only live terminals can be kept"));
    }
    t.keep = p.keep;
    let public = Public(t.clone());
    let lost = (!p.keep && t.holder.is_none()).then(|| t.reason.clone().unwrap_or_else(|| "holder_lost".into()));
    core.emit(&mut st, "keep", id, json!({"keep": p.keep}));
    if let Some(reason) = lost {
        core.record_finish(&mut st, id, proto::Exit { code: None, signal: None }, &reason);
        core.changed.notify_waiters();
    }
    core.save(&st);
    Ok(public)
}

pub(super) async fn terminal_resume(core: Ctx<'_>, p: ResumeParams) -> Result<Public, RpcError> {
    let id = resolve(core, &p.terminal)?;
    let (program, args) = p.argv.split_first().ok_or_else(|| RpcError::invalid_params("argv is empty"))?;
    let program = crate::config::ProgramName::try_from(program.clone()).map_err(RpcError::invalid_params)?;
    let line = crate::kept::command_line(&program, args).map_err(RpcError::invalid_params)?;
    record(core, id)?.check_program()?;
    let pgid = core.foreground(id).await;
    let Some(now) = (pgid > 0).then(|| crate::foreground::about(pgid)).flatten() else {
        return Err(RpcError::new(
            "unavailable",
            format!("{}'s foreground program could not be read", crate::terminals::address(id)),
            "retry while the program runs",
        ));
    };
    let mut st = core.lock();
    let t = st.reg.terminals.get_mut(&id).filter(|t| t.is_live()).ok_or_else(|| {
        RpcError::new("finished", format!("{} has finished", crate::terminals::address(id)), "see `hc ls --all`")
    })?;
    t.set_resume(crate::kept::Resume { line, cwd: now.cwd, pid: now.pid, ended_ms: None }, p.replace)?;
    let public = Public(t.clone());
    core.save(&st);
    Ok(public)
}
