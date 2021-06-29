use std::io;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct ChildProcess {
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    child: Rc<tokio::process::Child>,
}

impl AsyncRead for ChildProcess {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ChildProcess {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

impl ChildProcess {
    pub fn spawn_from_command(mut cmd: Command) -> Result<ChildProcess, std::io::Error> {
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let mut child = tokio::process::Command::from(cmd).spawn().expect("to async spawn process");

        Ok(ChildProcess {
            stdout: child.stdout.take().unwrap(),
            stdin: child.stdin.take().unwrap(),
            child: Rc::new(child),
        })
    }

    /// Returns a handle to the underlying `Child` process.
    /// Useful when waiting until child process exits.
    pub fn child(&self) -> Rc<tokio::process::Child> {
        Rc::clone(&self.child)
    }
}
