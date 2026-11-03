//! Linux and macOS: Unix sockets, PTYs, process groups, termios.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

// --- Connections -----------------------------------------------------------

/// A connection to a core or a holder (blocking).
pub type Stream = std::os::unix::net::UnixStream;
pub type Listener = std::os::unix::net::UnixListener;

pub fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path)
}

pub fn may_autostart(_explicit: bool) -> Result<(), crate::rpc::RpcError> {
    Ok(())
}

pub fn status_may_start_core() -> bool {
    true
}

pub fn connect_error(_error: &io::Error, _existing_only: bool) -> Option<crate::rpc::RpcError> {
    None
}

pub fn merge_env(
    env: &mut std::collections::BTreeMap<String, String>,
    extra: std::collections::BTreeMap<String, String>,
) {
    env.extend(extra);
}

pub fn listen(path: &Path) -> io::Result<Listener> {
    Listener::bind(path)
}

pub type AsyncRead = tokio::net::unix::OwnedReadHalf;
pub type AsyncWrite = tokio::net::unix::OwnedWriteHalf;

/// The core's socket for clients.
pub struct AsyncListener(tokio::net::UnixListener);

impl AsyncListener {
    /// Bind before the runtime runs (the core binds, then adopts, then serves).
    pub fn bind(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    pub fn serve(bound: std::os::unix::net::UnixListener) -> io::Result<AsyncListener> {
        Ok(AsyncListener(tokio::net::UnixListener::from_std(bound)?))
    }

    pub async fn accept(&mut self) -> io::Result<(AsyncRead, AsyncWrite)> {
        let (stream, _) = self.0.accept().await?;
        Ok(stream.into_split())
    }
}

pub fn remove_endpoint(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// --- Files -----------------------------------------------------------------

/// Default homes for state and config (XDG_STATE_HOME / XDG_CONFIG_HOME win).
pub fn data_dirs() -> (PathBuf, PathBuf) {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
    (home.join(".local/state"), home.join(".config"))
}

/// Where sockets go, and the prefix of their names: the user's runtime dir,
/// else beside the state.
/// The longest Unix socket path (macOS 103 bytes, Linux 107; one is the NUL).
pub const MAX_SOCKET_PATH: usize = {
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
};

pub fn endpoints(state: &Path) -> (PathBuf, String) {
    let run = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        // Logins without XDG_RUNTIME_DIR (some ssh setups) must still find the core.
        .or_else(|| Some(PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))).filter(|p| p.is_dir()));
    (run.map_or_else(|| state.to_path_buf(), |run| run.join("herdr-core")), String::new())
}

/// A directory only this user can enter.
pub fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Takes the lock on `file` unless someone else holds it.
pub fn try_lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

pub fn make_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
}

/// Same file, same build: device, inode, mtime, size.
pub fn file_identity(m: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}:{}:{}", m.dev(), m.ino(), m.mtime(), m.size())
}

// --- Processes -------------------------------------------------------------

/// Starts `cmd` in its own session (ending the terminal it was started from
/// won't end it), stdin/stdout to /dev/null, stderr to `log`.
///
/// It starts only once its starter has let go of it, as an orphan: endpoint
/// security software can give a program the treatment its parent gets when it
/// starts, and a core started from a shell it slows would pass that to every
/// terminal (seen on macOS: every process start in them took twice as long).
pub fn spawn_detached(mut cmd: Command, log: std::fs::File) -> io::Result<()> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            orphan()
        });
    }
    // The child we get only forks the program and exits.
    cmd.spawn()?.wait()?;
    Ok(())
}

/// In a forked child, before exec: forks again and returns in the grandchild
/// once the middle process has exited and it has a new parent. Only
/// async-signal-safe calls, as it runs between fork and exec.
fn orphan() -> io::Result<()> {
    let middle = unsafe { libc::getpid() };
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            while unsafe { libc::getppid() } == middle {
                unsafe { libc::usleep(1000) };
            }
            Ok(())
        }
        _ => unsafe { libc::_exit(0) },
    }
}

/// Puts the hc at `from` where `dest` is: a copy beside it that `works`,
/// then renamed over it (InvalidData: the copy doesn't run).
pub fn swap_in(from: &Path, dest: &Path, works: impl Fn(&Path) -> bool) -> io::Result<()> {
    let tmp = dest.with_file_name(format!(".herdr-core.new.{}", std::process::id()));
    let staged = std::fs::copy(from, &tmp).and_then(|_| make_executable(&tmp));
    if staged.is_err() || !works(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        staged?;
        return Err(io::ErrorKind::InvalidData.into());
    }
    std::fs::rename(&tmp, dest)
}

pub fn parent_pid() -> u32 {
    std::os::unix::process::parent_id()
}

pub fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

/// This machine's own shell, as a person here gets it.
pub fn own_shell() -> Vec<String> {
    shell_argv(std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into()))
}

/// `shell` as a person here starts it. On macOS a login shell, as Terminal and
/// iTerm start one: a core started from the Dock has launchd's bare PATH, and
/// only login profiles add the rest (path_helper's /etc/paths, Homebrew).
pub fn shell_argv(shell: String) -> Vec<String> {
    if cfg!(target_os = "macos") { vec![shell, "-l".into()] } else { vec![shell] }
}

/// One argument as `own_shell` reads it back, typed at its prompt. sh, bash,
/// zsh, fish and nu all take single quotes literally, except that fish reads
/// `\\` and `\'` in them and tcsh expands `!`: no quoting reads those back
/// the same in all of them, so there is none.
pub fn quote_arg(arg: &str) -> Option<String> {
    (!arg.contains(['\'', '\\', '!'])).then(|| format!("'{arg}'"))
}

/// A shell line as this machine runs one.
pub fn shell_line(line: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), line.into()]
}

/// Returns once the child exited, leaving it unreaped: until it is reaped
/// its pid, and so its group id, can't go to another process.
pub async fn exited(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    let _ = tokio::task::spawn_blocking(move || {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let done = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0;
            if done || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return;
            }
        }
    })
    .await;
}

/// An exec starts its own process group, with no signals blocked (see `unblock_signals`).
pub fn exec_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
    // SAFETY: unblock_signals only makes async-signal-safe calls.
    unsafe {
        cmd.pre_exec(|| unblock_signals());
    }
}

/// For a `pre_exec` hook: lets the program about to run receive every signal. Rust passes a
/// process's blocked signals on to what it starts, and whatever started the core may have
/// blocked some, which would leave programs here unable to see SIGHUP or SIGTERM: only SIGKILL
/// could end them, so they could not clean up, and the sweep would wait for its last step.
///
/// # Safety
/// Only async-signal-safe calls, as a `pre_exec` hook needs.
unsafe fn unblock_signals() -> io::Result<()> {
    unsafe {
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        match libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut()) {
            0 => Ok(()),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    }
}

/// An exec's process group: it and what it starts, unless they leave it.
#[derive(Clone)]
pub struct ExecGroup(i32);

impl ExecGroup {
    pub fn new(child: &tokio::process::Child) -> io::Result<ExecGroup> {
        Ok(ExecGroup(child.id().map_or(0, |pid| pid as i32)))
    }

    pub fn end(&self) {
        self.signal(libc::SIGTERM);
    }

    pub fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    fn signal(&self, signal: i32) {
        if self.0 > 0 {
            unsafe { libc::kill(-self.0, signal) };
        }
    }
}

pub fn random_bytes(bytes: &mut [u8]) {
    File::open("/dev/urandom").and_then(|mut f| io::Read::read_exact(&mut f, bytes)).expect("/dev/urandom readable");
}

/// Resolves when the process is asked to end (SIGTERM): a planned restart.
pub async fn termination() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut term) => drop(term.recv().await),
        Err(_) => std::future::pending().await,
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// This system's own id, new on a cloned disk: Linux's machine-id (made per
/// box at first boot), a Mac's hardware UUID. None when there is none or it
/// can't be read. One source only: a second one that disagrees, read when
/// the first fails once, would look like a copy and forget live terminals.
pub fn machine_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let line = text.lines().find(|l| l.contains("\"IOPlatformUUID\""))?;
        line.rsplit('"').nth(1).map(String::from)
    }
    #[cfg(not(target_os = "macos"))]
    std::fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "uninitialized")
}

// --- Terminals -------------------------------------------------------------

/// A program on its own terminal.
pub struct Pty {
    master: File,
}

pub type Process = Child;

/// Starts `argv` on a new terminal of `cols`x`rows`.
pub fn spawn_pty(
    argv: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> io::Result<(Pty, Process)> {
    let program = argv.first().ok_or_else(|| io::Error::other("empty argv"))?;
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut size) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            unblock_signals()
        });
    }
    let child = cmd.spawn()?;
    Ok((Pty { master }, child))
}

impl Pty {
    /// Its output, read until every program on it has gone.
    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn writer(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
    }

    /// The process group in the foreground (a shell's job), or -1.
    pub fn foreground(&self) -> i32 {
        unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) }
    }

    /// The program's group, plus whatever group is in the foreground.
    pub fn signal(&self, pid: u32, signal: i32) {
        unsafe {
            libc::killpg(pid as i32, signal);
            let fg = libc::tcgetpgrp(self.master.as_raw_fd());
            if fg > 0 && fg != pid as i32 {
                libc::killpg(fg, signal);
            }
        }
    }

    /// Its program is gone: hang up what still holds the terminal.
    pub fn hang_up(&self, pid: u32) {
        unsafe { libc::killpg(pid as i32, libc::SIGHUP) };
    }
}

/// Waits for it to end: (exit code, signal).
pub fn wait(child: &mut Process) -> io::Result<(Option<i32>, Option<i32>)> {
    loop {
        match child.wait() {
            Ok(status) => return Ok((status.code(), status.signal())),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

// --- The console (attach, picker) --------------------------------------------

pub mod console {
    use super::*;

    static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        TERMINATED.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// A kill (TERM, HUP, INT) only sets a flag: the input loop sees it within one
    /// poll and leaves through the normal cleanup, so the terminal is given back.
    pub fn catch_termination() {
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
        }
    }

    pub fn terminated() -> bool {
        TERMINATED.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub struct RawMode(Option<libc::termios>);

    impl RawMode {
        pub fn enter() -> RawMode {
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
                return RawMode(None);
            }
            let mut raw = saved;
            unsafe {
                libc::cfmakeraw(&mut raw);
                libc::tcsetattr(0, libc::TCSANOW, &raw);
            }
            RawMode(Some(saved))
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            if let Some(saved) = &self.0 {
                unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
            }
        }
    }

    /// Unbuffered stdin/stdout: std's stdout holds bytes until a newline.
    pub fn stdin() -> std::mem::ManuallyDrop<File> {
        std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(0) })
    }

    pub fn stdout() -> std::mem::ManuallyDrop<File> {
        std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(1) })
    }

    pub fn out(bytes: &[u8]) {
        let _ = stdout().write_all(bytes);
    }

    pub fn window_size() -> (u16, u16) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }

    /// Waits up to `ms` for stdin; returns what arrived (empty: nothing yet,
    /// None: stdin is closed).
    pub fn read_stdin(ms: i32) -> Option<Vec<u8>> {
        let mut fds = [libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 }];
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) } <= 0 {
            return Some(Vec::new());
        }
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        (n > 0).then(|| buf[..n as usize].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    /// A shell that sends itself SIGTERM, then exits 7 if that did nothing.
    const TERMINATES_ITSELF: [&str; 3] = ["/bin/sh", "-c", "kill -TERM $$; sleep 1; exit 7"];

    /// Runs `body` with SIGTERM blocked on this thread, as when something that started the
    /// core blocked it, then blocks it no more.
    fn with_sigterm_blocked<T>(body: impl FnOnce() -> T) -> T {
        let block = |how: libc::c_int| unsafe {
            let mut term: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut term);
            libc::sigaddset(&mut term, libc::SIGTERM);
            assert_eq!(libc::pthread_sigmask(how, &term, std::ptr::null_mut()), 0);
        };
        block(libc::SIG_BLOCK);
        let result = body();
        block(libc::SIG_UNBLOCK);
        result
    }

    #[test]
    fn a_terminals_program_gets_signals_its_starter_blocked() {
        let argv: Vec<String> = TERMINATES_ITSELF.map(String::from).to_vec();
        let env = [("PATH".to_string(), "/usr/bin:/bin".to_string())].into();
        let (_pty, mut child) = with_sigterm_blocked(|| super::spawn_pty(&argv, &env, "/", 80, 24).unwrap());
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    }

    #[tokio::test]
    async fn an_exec_gets_signals_its_starter_blocked() {
        let mut cmd = tokio::process::Command::new(TERMINATES_ITSELF[0]);
        cmd.args(&TERMINATES_ITSELF[1..]);
        super::exec_group(&mut cmd);
        let mut child = with_sigterm_blocked(|| cmd.spawn().unwrap());
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn a_persons_shell_is_a_login_shell_on_macos_only() {
        let expected: &[&str] = if cfg!(target_os = "macos") { &["/bin/zsh", "-l"] } else { &["/bin/zsh"] };
        assert_eq!(super::shell_argv("/bin/zsh".into()), expected);
    }

    #[test]
    fn a_detached_program_starts_after_its_starter_has_let_go_of_it() {
        let dir =
            std::env::temp_dir().join(format!("hc-detached-{}-{}", std::process::id(), super::super::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let seen = dir.join("ppid");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "echo $PPID > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"", "sh"]).arg(&seen);
        super::spawn_detached(cmd, std::fs::File::create(dir.join("log")).unwrap()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !seen.exists() {
            assert!(std::time::Instant::now() < deadline, "the detached program never ran");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ppid: u32 = std::fs::read_to_string(&seen).unwrap().trim().parse().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_ne!(ppid, std::process::id(), "it ran as the starter's child");
    }

    #[test]
    fn a_detached_program_that_cannot_start_is_an_error() {
        let log =
            std::env::temp_dir().join(format!("hc-detached-{}-{}.log", std::process::id(), super::super::random_hex()));
        let cmd = std::process::Command::new("/nonexistent/hc-test-program");
        let started = super::spawn_detached(cmd, std::fs::File::create(&log).unwrap());
        std::fs::remove_file(&log).unwrap();
        assert_eq!(started.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn arguments_any_unix_shell_reads_differently_are_not_quoted() {
        assert_eq!(super::quote_arg("my notes.txt; $HOME *").as_deref(), Some("'my notes.txt; $HOME *'"));
        for arg in ["it's", "a\\b", "wow!"] {
            assert_eq!(super::quote_arg(arg), None, "{arg:?}");
        }
    }
}
