//! The `towerminer.exe` child process: start, read, stop.

use crate::protocol::{self, Event, Line};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

pub const MINER_EXE: &str = "towerminer.exe";
const MAX_LINE: usize = 4000;

pub enum Msg {
    Event(Event),
    Log(String),
    Closed,
}

pub struct Miner {
    child: Child,
    pub pid: u32,
    /// Windows job object with KILL_ON_JOB_CLOSE: if this program dies in
    /// any way, Windows closes the handle and kills the miner with it.
    #[cfg(windows)]
    _job: Option<job::Job>,
}

/// `towerminer.exe` in the folder of this program.
pub fn miner_path() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(MINER_EXE))
}

impl Miner {
    /// Spawn the miner; its stdout (JSON events) and stderr (log) are read on
    /// two threads that feed the returned channel. `wake` is called after
    /// every message so the window repaints.
    pub fn start(
        exe: &Path,
        args: &[String],
        key: &str,
        wake: impl Fn() + Send + Clone + 'static,
    ) -> Result<(Miner, Receiver<Msg>, Vec<String>), String> {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        // The key travels in the environment, never on the command line.
        if key.is_empty() {
            cmd.env_remove("TOWERMINER_KEY");
        } else {
            cmd.env("TOWERMINER_KEY", key);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", exe.display()))?;
        let pid = child.id();
        #[allow(unused_mut)]
        let mut notes = Vec::new();

        #[cfg(windows)]
        let job = {
            let j = job::Job::kill_on_close();
            match &j {
                Some(j) if j.assign(&child) => {}
                _ => notes.push(
                    "warning: could not tie the miner to this window (job object); \
                     it is still stopped by Stop and on exit"
                        .to_string(),
                ),
            }
            j
        };

        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        spawn_reader(stdout, tx.clone(), wake.clone(), true);
        spawn_reader(stderr, tx, wake, false);
        Ok((
            Miner {
                child,
                pid,
                #[cfg(windows)]
                _job: job,
            },
            rx,
            notes,
        ))
    }

    /// Exit status once the process has ended.
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Kill the process and reap it.
    pub fn stop(&mut self) {
        if self.try_wait().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_reader(
    pipe: impl Read + Send + 'static,
    tx: Sender<Msg>,
    wake: impl Fn() + Send + 'static,
    json: bool,
) {
    std::thread::spawn(move || {
        let mut r = BufReader::new(pipe);
        let mut buf = Vec::with_capacity(512);
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf);
            let kind = if json { protocol::classify(&line) } else { Line::Text };
            let msg = match kind {
                Line::Event(ev) => Msg::Event(ev),
                Line::OtherJson => continue,
                // stderr, or stdout that is not JSON: shown in the log.
                Line::Text => match clean_line(&line) {
                    Some(l) => Msg::Log(l),
                    None => continue,
                },
            };
            if tx.send(msg).is_err() {
                return;
            }
            wake();
        }
        let _ = tx.send(Msg::Closed);
        wake();
    });
}

/// Strip terminal colour codes and control characters; `None` for a blank line.
pub fn clean_line(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ params final-byte(0x40..=0x7e); other escapes: drop ESC + next.
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            } else {
                it.next();
            }
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
        if out.len() > MAX_LINE {
            out.push_str(" [...]");
            break;
        }
    }
    let t = out.trim_end();
    (!t.trim().is_empty()).then(|| t.to_string())
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    impl Job {
        pub fn kill_on_close() -> Option<Job> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Job(h))
            }
        }

        pub fn assign(&self, child: &Child) -> bool {
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clean_line;

    #[test]
    fn strips_colours_and_controls() {
        assert_eq!(clean_line("\x1b[31mhot 81 C\x1b[0m\r\n").as_deref(), Some("hot 81 C"));
        assert_eq!(clean_line("  \r\n"), None);
        assert_eq!(clean_line("a\tb").as_deref(), Some("a    b"));
        assert_eq!(clean_line("\x1b[1;33mwarn\x1b[0m: x").as_deref(), Some("warn: x"));
        let long = "x".repeat(10_000);
        assert!(clean_line(&long).unwrap().len() < 4100);
    }
}
