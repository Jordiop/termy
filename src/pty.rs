//! Cross-platform `portable-pty` wrapper (Unix PTY on macOS/Linux, ConPTY on
//! Windows): spawn in a given cwd with extra env vars, background reader /
//! writer / waiter threads, per-pane resize.
//!
//! Nothing here ever blocks the caller: output is pushed to a callback from
//! the reader thread, input is queued to the writer thread, and the child's
//! exit is reported by a dedicated waiter thread (on Windows the reader may
//! never see EOF after the child exits, so we don't rely on it).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread;

use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// What to run and where.
#[derive(Clone, Debug)]
pub struct SpawnOptions {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// How the child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: u32,
    pub success: bool,
}

/// `(program, args)` of the platform's default interactive shell.
pub fn default_shell() -> (String, Vec<String>) {
    if cfg!(windows) {
        if find_in_path("pwsh.exe").is_some() {
            return ("pwsh.exe".into(), vec!["-NoLogo".into()]);
        }
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        return (comspec, Vec::new());
    }
    let from_env = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    if cfg!(target_os = "macos") {
        // GUI apps get a minimal environment; a login shell loads the user's
        // profile (PATH from Homebrew, etc.), like Terminal.app does.
        (from_env.unwrap_or_else(|| "/bin/zsh".into()), vec!["-l".into()])
    } else {
        let fallback = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        (from_env.unwrap_or_else(|| fallback.into()), Vec::new())
    }
}

/// Locates an executable on `$PATH` (`PATHEXT`-agnostic: pass the full name).
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// A running child attached to a pseudo-terminal.
pub struct PtyHandle {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer_tx: Sender<Vec<u8>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    #[cfg_attr(not(test), allow(dead_code))]
    pid: Option<u32>,
}

impl PtyHandle {
    /// Spawns `opts` on a new PTY.
    ///
    /// * `on_output` runs on the reader thread for every chunk of output.
    /// * `on_exit` runs once on the waiter thread when the child exits.
    pub fn spawn(
        opts: &SpawnOptions,
        mut on_output: impl FnMut(&[u8]) + Send + 'static,
        on_exit: impl FnOnce(ExitInfo) + Send + 'static,
    ) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(pty_size(opts.cols, opts.rows))
            .map_err(|e| format!("openpty failed: {e}"))?;

        let mut cmd = CommandBuilder::new(&opts.program);
        cmd.args(&opts.args);
        if let Some(cwd) = opts.cwd.as_ref().filter(|p| p.is_dir()) {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", crate::config::APP_NAME);
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("could not start `{}`: {e}", opts.program))?;
        // The child holds its own copy; dropping ours lets EOF propagate.
        drop(pair.slave);

        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let mut writer = pair.master.take_writer().map_err(|e| e.to_string())?;

        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => on_output(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        let (writer_tx, writer_rx) = unbounded::<Vec<u8>>();
        thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
                for chunk in writer_rx {
                    if writer.write_all(&chunk).and_then(|_| writer.flush()).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        thread::Builder::new()
            .name("pty-waiter".into())
            .spawn(move || {
                let info = match child.wait() {
                    Ok(status) => ExitInfo { code: status.exit_code(), success: status.success() },
                    Err(_) => ExitInfo { code: u32::MAX, success: false },
                };
                on_exit(info);
            })
            .map_err(|e| e.to_string())?;

        Ok(Self { master: Mutex::new(pair.master), writer_tx, killer: Mutex::new(killer), pid })
    }

    /// Queues bytes for the child's stdin. Never blocks.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.writer_tx.send(bytes.into());
    }

    /// A cloneable handle to the input queue (used for terminal replies such
    /// as cursor-position reports).
    pub fn input_sender(&self) -> Sender<Vec<u8>> {
        self.writer_tx.clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Err(e) = self.master.lock().resize(pty_size(cols, rows)) {
            log::warn!("pty resize to {cols}x{rows} failed: {e}");
        }
    }

    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().kill();
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Never leave orphaned shells/agents behind.
        self.kill();
    }
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Runs a command headlessly, returning (output, exit info).
    fn run(opts: SpawnOptions, input: Option<&[u8]>) -> (String, ExitInfo) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let (exit_tx, exit_rx) = crossbeam_channel::bounded(1);
        let out = output.clone();
        let pty = PtyHandle::spawn(
            &opts,
            move |bytes| out.lock().extend_from_slice(bytes),
            move |info| {
                let _ = exit_tx.send(info);
            },
        )
        .expect("spawn");
        if let Some(input) = input {
            pty.write(input.to_vec());
        }
        let info = exit_rx.recv_timeout(Duration::from_secs(10)).expect("child did not exit");
        // Give the reader a moment to drain the last bytes.
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let text = String::from_utf8_lossy(&output.lock()).into_owned();
        (text, info)
    }

    fn sh(script: &str) -> SpawnOptions {
        if cfg!(windows) {
            SpawnOptions {
                program: "cmd.exe".into(),
                args: vec!["/C".into(), script.into()],
                cwd: None,
                env: vec![],
                cols: 80,
                rows: 24,
            }
        } else {
            SpawnOptions {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: None,
                env: vec![],
                cols: 80,
                rows: 24,
            }
        }
    }

    #[test]
    fn runs_command_headless_and_reports_exit() {
        let (out, info) = run(sh("echo hello-from-pty"), None);
        assert!(out.contains("hello-from-pty"), "output: {out:?}");
        assert!(info.success);
    }

    #[test]
    fn reports_failure_exit_code() {
        let (_, info) = run(sh("exit 3"), None);
        assert_eq!(info, ExitInfo { code: 3, success: false });
    }

    #[cfg(unix)]
    #[test]
    fn env_cwd_and_term_are_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = sh("echo \"$TERM|$TERM_PROGRAM|$MY_VAR|$(pwd -P)\"");
        opts.cwd = Some(dir.path().to_path_buf());
        opts.env.push(("MY_VAR".into(), "42".into()));
        let (out, _) = run(opts, None);
        let expected_dir = dir.path().canonicalize().unwrap();
        let expected = format!("xterm-256color|termy|42|{}", expected_dir.display());
        assert!(out.contains(&expected), "output: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn writes_reach_the_child() {
        let (out, info) = run(sh("read line; echo \"got:$line\""), Some(b"ping\n"));
        assert!(out.contains("got:ping"), "output: {out:?}");
        assert!(info.success);
    }

    #[cfg(unix)]
    #[test]
    fn resize_is_visible_to_the_child() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let out = output.clone();
        let (exit_tx, exit_rx) = crossbeam_channel::bounded(1);
        let pty = PtyHandle::spawn(
            &sh("read _; stty size"),
            move |b| out.lock().extend_from_slice(b),
            move |i| {
                let _ = exit_tx.send(i);
            },
        )
        .unwrap();
        pty.resize(132, 43);
        pty.write(b"\n".to_vec());
        exit_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        thread::sleep(Duration::from_millis(200));
        let text = String::from_utf8_lossy(&output.lock()).into_owned();
        assert!(text.contains("43 132"), "output: {text:?}");
    }

    #[test]
    fn kill_terminates_child() {
        let (exit_tx, exit_rx) = crossbeam_channel::bounded(1);
        let script = if cfg!(windows) { "ping -n 30 127.0.0.1 >NUL" } else { "sleep 30" };
        let pty = PtyHandle::spawn(&sh(script), |_| {}, move |i| {
            let _ = exit_tx.send(i);
        })
        .unwrap();
        assert!(pty.pid().is_some());
        pty.kill();
        let info = exit_rx.recv_timeout(Duration::from_secs(5)).expect("killed child must exit");
        assert!(!info.success);
    }

    #[test]
    fn missing_program_is_an_error() {
        let mut opts = sh("");
        opts.program = "definitely-not-a-real-program-xyz".into();
        opts.args.clear();
        assert!(PtyHandle::spawn(&opts, |_| {}, |_| {}).is_err());
    }

    #[test]
    fn default_shell_is_sensible() {
        let (program, _) = default_shell();
        assert!(!program.is_empty());
    }
}
