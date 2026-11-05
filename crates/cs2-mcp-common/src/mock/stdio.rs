//! Drive a stdio MCP server binary line by line (for the servers' end-to-end tests).
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

pub struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    /// every stdout line seen so far (tests assert they are all JSON-RPC frames)
    pub stdout_lines: Vec<String>,
    /// stderr is drained by a task from the start: a server that logs more than a pipe buffer
    /// (the DEBUG request dumps) would otherwise block mid-test with nobody reading it
    stderr: Option<tokio::task::JoinHandle<String>>,
}

impl Server {
    pub async fn spawn(bin: &str, args: &[&str]) -> Server {
        let mut child = tokio::process::Command::new(bin)
            .args(args)
            .env("RUST_LOG", "debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut stderr_pipe = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr_pipe, &mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        });
        Server {
            child,
            stdin,
            stdout,
            stdout_lines: Vec::new(),
            stderr: Some(stderr),
        }
    }

    async fn send(&mut self, v: Value) {
        self.stdin
            .write_all(format!("{v}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// Send a request and read frames until the reply with this id arrives.
    pub async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(20), self.stdout.next_line())
                .await
                .expect("reply within 20 s")
                .unwrap()
                .expect("server closed stdout");
            self.stdout_lines.push(line.clone());
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("non-JSON on stdout: {line:?}"));
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
        }
    }

    /// Close stdin, wait for exit: (exit code, stdout lines seen, stderr text).
    pub async fn close(mut self) -> (Option<i32>, Vec<String>, String) {
        drop(self.stdin);
        // the child exits first (its stderr closes with it), then the drain task is joined
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("exit after EOF")
            .unwrap();
        let stderr = match self.stderr.take() {
            Some(task) => tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("stderr drained after exit")
                .unwrap_or_default(),
            None => String::new(),
        };
        while let Ok(Some(l)) = self.stdout.next_line().await {
            self.stdout_lines.push(l);
        }
        (status.code(), self.stdout_lines, stderr)
    }
}
