//! Bounded child processes with retained stdout and stderr.
use crate::model::Reply;
use soldr_platform::sync::Mutex;
use std::{sync::Arc, time::Duration};
use tokio::process::Command;

static EXECUTABLE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_executable(path: std::path::PathBuf) -> Result<(), &'static str> {
    EXECUTABLE
        .set(path)
        .map_err(|_| "UI executable is already configured")
}

pub async fn command(args: &[&str]) -> Reply {
    use std::process::Stdio;
    let mut child = match Command::new(EXECUTABLE.get().expect("configured by soldr ui"))
        .args(args)
        .env(crate::COMMAND_EDGE_ENV, "1")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return Reply {
                stderr: format!("Could not launch soldr: {e}"),
                ..Reply::default()
            }
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out = capture(stdout);
    let err = capture(stderr);
    let status = tokio::time::timeout(Duration::from_secs(35), child.wait()).await;
    let (success, diagnostic) = match status {
        Ok(Ok(status)) => (
            status.success(),
            if status.success() {
                String::new()
            } else {
                format!("\nsoldr {} exited {status}", args.join(" "))
            },
        ),
        Ok(Err(e)) => (false, format!("Could not wait for soldr: {e}")),
        Err(e) => {
            let kill = child.kill().await;
            (
                false,
                format!(
                    "soldr {} timed out after 35s: {e}; cleanup: {kill:?}",
                    args.join(" ")
                ),
            )
        }
    };
    let (stdout, out_error) = drain(out).await;
    let (stderr, err_error) = drain(err).await;
    Reply {
        success: success && out_error.is_empty() && err_error.is_empty(),
        stdout,
        stderr: format!("{stderr}{diagnostic}{out_error}{err_error}"),
    }
}

type Capture = (
    tokio::task::JoinHandle<std::io::Result<()>>,
    Arc<Mutex<Vec<u8>>>,
);

fn capture(mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> Capture {
    use tokio::io::AsyncReadExt;
    let data = Arc::new(Mutex::new(Vec::new()));
    let output = data.clone();
    let task = tokio::spawn(async move {
        let mut chunk = [0; 8192];
        loop {
            let size = pipe.read(&mut chunk).await?;
            if size == 0 {
                return Ok(());
            }
            output.lock().extend_from_slice(&chunk[..size]);
        }
    });
    (task, data)
}

async fn drain((mut task, data): Capture) -> (String, String) {
    let diagnostic = match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
        Ok(Ok(Ok(()))) => String::new(),
        Ok(Ok(Err(e))) => format!("Output read failed: {e}"),
        Ok(Err(e)) => format!("Output task failed: {e}"),
        Err(e) => {
            task.abort();
            format!("Output pipe stayed open after child exit: {e}; partial output retained")
        }
    };
    let text = String::from_utf8_lossy(&data.lock()).into();
    (text, diagnostic)
}
