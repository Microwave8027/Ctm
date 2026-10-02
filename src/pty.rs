//! Runs interactive `claude` commands (login, `--cloud`) inside a pseudo
//! terminal so ctm can read what they print and type answers for the user.

use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use regex::Regex;
use tokio::sync::{mpsc, oneshot};

pub struct PtyProcess {
    // Keeping the master alive keeps the terminal open.
    _master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub struct Spawned {
    pub process: Arc<PtyProcess>,
    /// Raw output chunks, escape sequences included.
    pub output: mpsc::UnboundedReceiver<String>,
    /// Exit code once the process ends.
    pub exit: oneshot::Receiver<u32>,
}

impl PtyProcess {
    /// Starts `program args...` in a wide terminal (so URLs don't wrap).
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        env_remove: &[&str],
    ) -> Result<Spawned> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 50,
                cols: 1000,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pseudo terminal")?;
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        // Never try to launch a browser on the server.
        cmd.env("BROWSER", "true");
        for name in env_remove {
            cmd.env_remove(name);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {program}"))?;
        drop(pair.slave);

        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out_tx
                            .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        let (exit_tx, exit_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
            let _ = exit_tx.send(code);
        });

        Ok(Spawned {
            process: Arc::new(PtyProcess {
                _master: Mutex::new(pair.master),
                writer: Mutex::new(writer),
                killer: Mutex::new(killer),
            }),
            output: out_rx,
            exit: exit_rx,
        })
    }

    /// Types `text` followed by Enter.
    pub fn type_line(&self, text: &str) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(text.as_bytes())?;
        w.flush()?;
        // Send Enter separately so TUIs don't treat it as part of a paste.
        std::thread::sleep(std::time::Duration::from_millis(150));
        w.write_all(b"\r")?;
        w.flush()?;
        Ok(())
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().unwrap().kill();
    }
}

/// Removes terminal escape sequences (CSI, OSC, and two-byte escapes) and
/// turns carriage returns into newlines.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // CSI: parameters then a final byte in @..~
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: until BEL or ESC \
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => out.push('\n'),
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

/// First hyperlink target (OSC 8) in raw terminal output, if any.
pub fn first_hyperlink(raw: &str) -> Option<String> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"\x1b\]8;[^;\x07\x1b]*;(https://[^\x07\x1b]+)").unwrap());
    re.captures(raw).map(|c| c[1].to_string())
}

/// Collects terminal output into de-duplicated, non-empty lines. TUIs
/// redraw the same text many times; each distinct line is reported once.
#[derive(Default)]
pub struct ScreenLines {
    pending: String,
    seen: std::collections::HashSet<String>,
}

impl ScreenLines {
    pub fn push(&mut self, raw: &str) -> Vec<String> {
        self.pending.push_str(&strip_ansi(raw));
        let mut out = Vec::new();
        while let Some(i) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=i).collect();
            let line = line.trim().to_string();
            if !line.is_empty() && self.seen.len() < 10_000 && self.seen.insert(line.clone()) {
                out.push(line);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes() {
        let raw = "\x1b[1mBold\x1b[0m\r\n\x1b]8;id=1;https://x.test/a?b=1\x1b\\link\x1b]8;;\x1b\\ done\x07";
        assert_eq!(strip_ansi(raw), "Bold\n\nlink done");
        assert_eq!(
            first_hyperlink(raw).as_deref(),
            Some("https://x.test/a?b=1")
        );
    }

    #[test]
    fn screen_lines_dedupe() {
        let mut s = ScreenLines::default();
        assert_eq!(s.push("hello\nwor"), vec!["hello"]);
        assert_eq!(s.push("ld\nhello\n"), vec!["world"]);
    }

    #[test]
    fn pty_round_trip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut sp = PtyProcess::spawn(
                "sh",
                &["-c".into(), "printf 'code? '; read x; echo got:$x".into()],
                Path::new("."),
                &[],
                &[],
            )
            .unwrap();
            let mut text = String::new();
            while !text.contains("code?") {
                text.push_str(&sp.output.recv().await.unwrap());
            }
            sp.process.type_line("abc").unwrap();
            while !strip_ansi(&text).contains("got:abc") {
                match sp.output.recv().await {
                    Some(c) => text.push_str(&c),
                    None => break,
                }
            }
            assert!(strip_ansi(&text).contains("got:abc"), "{text:?}");
            assert_eq!(sp.exit.await.unwrap(), 0);
        });
    }
}
