use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eyre::{Result, ensure, eyre};
use serde::Serialize;

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct Output {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
}

impl Output {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

pub struct ManagedChild {
    stopped: bool,
    child: Child,
    stdout: Receiver<(String, bool)>,
    stderr: Receiver<(String, bool)>,
}

fn capture(mut pipe: impl Read + Send + 'static) -> Receiver<(String, bool)> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut truncated = false;
        let mut buffer = [0; 8192];
        while let Ok(count) = pipe.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.len()));
            output.extend_from_slice(&buffer[..keep]);
            truncated |= keep < count;
        }
        let _ = sender.send((String::from_utf8_lossy(&output).into_owned(), truncated));
    });
    receiver
}

impl ManagedChild {
    pub fn spawn(
        program: &Path,
        args: &[OsString],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Result<Self> {
        ensure!(
            cfg!(unix),
            "ARC process supervision currently requires Unix"
        );
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let stdout = capture(
            child
                .stdout
                .take()
                .ok_or_else(|| eyre!("Missing stdout pipe"))?,
        );
        let stderr = capture(
            child
                .stderr
                .take()
                .ok_or_else(|| eyre!("Missing stderr pipe"))?,
        );
        Ok(Self {
            stopped: false,
            child,
            stdout,
            stderr,
        })
    }

    pub fn running(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    fn signal_group(&self, signal: &str) {
        #[cfg(unix)]
        {
            let _ = Command::new("/bin/kill")
                .args([signal, "--", &format!("-{}", self.child.id())])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.signal_group("-TERM");
        let end = Instant::now() + Duration::from_millis(200);
        while Instant::now() < end && self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.signal_group("-KILL");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn finish(mut self, deadline: Instant) -> Result<Output> {
        let mut timed_out = false;
        loop {
            if !self.running()? {
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.stop();
        let status = self.child.wait()?;
        let (stdout, stdout_truncated) = self
            .stdout
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or(("Output pipe did not close".into(), true));
        let (stderr, stderr_truncated) = self
            .stderr
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or(("Error pipe did not close".into(), true));
        Ok(Output {
            exit_code: status.code(),
            timed_out,
            stdout,
            stderr,
            output_truncated: stdout_truncated || stderr_truncated,
        })
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn run(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    env: &[(OsString, OsString)],
    deadline: Instant,
) -> Result<Output> {
    ensure!(
        Instant::now() < deadline,
        "Execution budget exhausted before spawning process"
    );
    ManagedChild::spawn(program, args, cwd, env)?.finish(deadline)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn nonzero_exit_is_not_success() {
        let output = run(
            Path::new("/bin/sh"),
            &["-c".into(), "printf failure >&2; exit 7".into()],
            Path::new("/tmp"),
            &[],
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert_eq!(output.stderr, "failure");
        assert!(!output.succeeded());
    }

    #[test]
    fn exhausted_budget_never_launches_command() {
        assert!(
            run(
                Path::new("/does/not/exist"),
                &[],
                Path::new("/tmp"),
                &[],
                Instant::now()
            )
            .unwrap_err()
            .to_string()
            .contains("budget")
        );
    }

    #[test]
    fn timeout_reaps_owned_process() {
        let start = Instant::now();
        let output = run(
            Path::new("/bin/sh"),
            &["-c".into(), "while :; do :; done".into()],
            Path::new("/tmp"),
            &[],
            start + Duration::from_millis(100),
        )
        .unwrap();
        assert!(output.timed_out);
        assert!(!output.succeeded());
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
