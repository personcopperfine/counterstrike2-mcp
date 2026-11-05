//! Fake Source RCON server for tests: a loopback listener that answers the auth and exec packets
//! of [`crate::rcon`] and records every command it runs.
//!
//! Modes (what the server does, as seen on a live CS2 dedicated server):
//! * [`Mode::Sentinel`]: classic. Auth sends an empty `type 0` then the `type 2` reply; a command
//!   gets its reply and the sentinel's reply (the `\0\1\0\0` filler body).
//! * [`Mode::SentinelIgnored`]: auth as classic, but the sentinel is not answered.
//! * [`Mode::LoneAuthResponse`]: CS2 as observed live: auth is a single `type 2` packet, and the
//!   sentinel is not answered.
//!
//! In every mode `cvarlist` replies with three packets (the multi-packet path); other commands
//! reply `ran <command>`; a wrong password gets the rejection id. Each accepted connection is
//! served until it closes, so a client may reconnect (the tools open one connection per call).
use crate::rcon::{
    encode, read_packet, REJECTED_ID, SERVERDATA_AUTH, SERVERDATA_AUTH_RESPONSE,
    SERVERDATA_EXECCOMMAND, SERVERDATA_RESPONSE_VALUE,
};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Sentinel,
    SentinelIgnored,
    LoneAuthResponse,
}

/// Commands the server ran (after a successful auth), shared with the test.
pub type CommandLog = Arc<Mutex<Vec<String>>>;

pub struct FakeRcon {
    /// `127.0.0.1:<port>` the server listens on
    pub addr: String,
    pub port: u16,
    commands: CommandLog,
    accept: JoinHandle<()>,
}

impl FakeRcon {
    /// Listen on an ephemeral loopback port, classic mode.
    pub async fn start(password: &str) -> FakeRcon {
        Self::bind("127.0.0.1:0", password, Mode::Sentinel, Arc::default()).await
    }

    /// Listen on `addr` (for example `127.0.0.1:27155`) in `mode`, recording into `commands`.
    pub async fn bind(addr: &str, password: &str, mode: Mode, commands: CommandLog) -> FakeRcon {
        let listener = TcpListener::bind(addr).await.expect("bind fake rcon");
        let port = listener.local_addr().expect("fake rcon addr").port();
        let password = password.to_string();
        let recorded = commands.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(sock, password.clone(), mode, recorded.clone()));
            }
        });
        FakeRcon {
            addr: format!("127.0.0.1:{port}"),
            port,
            commands,
            accept,
        }
    }

    /// Every command that was executed (after a successful auth), oldest first.
    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("fake rcon log").clone()
    }
}

impl Drop for FakeRcon {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve(mut sock: TcpStream, password: String, mode: Mode, commands: CommandLog) {
    let mut authed = false;
    loop {
        let Ok(p) = read_packet(&mut sock).await else {
            return;
        };
        let reply: Vec<u8> = match p.ty {
            SERVERDATA_AUTH if p.body == password => {
                authed = true;
                let mut v = Vec::new();
                if mode != Mode::LoneAuthResponse {
                    v.extend(encode(p.id, SERVERDATA_RESPONSE_VALUE, ""));
                }
                v.extend(encode(p.id, SERVERDATA_AUTH_RESPONSE, ""));
                v
            }
            SERVERDATA_AUTH => encode(REJECTED_ID, SERVERDATA_AUTH_RESPONSE, ""),
            SERVERDATA_EXECCOMMAND if !authed => encode(REJECTED_ID, SERVERDATA_AUTH_RESPONSE, ""),
            SERVERDATA_EXECCOMMAND => {
                commands.lock().expect("fake rcon log").push(p.body.clone());
                match p.body.as_str() {
                    "cvarlist" => [
                        encode(p.id, SERVERDATA_RESPONSE_VALUE, "sv_cheats "),
                        encode(p.id, SERVERDATA_RESPONSE_VALUE, "mp_freezetime "),
                        encode(p.id, SERVERDATA_RESPONSE_VALUE, "bot_quota"),
                    ]
                    .concat(),
                    other => encode(p.id, SERVERDATA_RESPONSE_VALUE, &format!("ran {other}")),
                }
            }
            // the empty sentinel request: answered only by the classic server
            SERVERDATA_RESPONSE_VALUE if mode == Mode::Sentinel => {
                encode(p.id, SERVERDATA_RESPONSE_VALUE, "\0\u{1}\0\0")
            }
            SERVERDATA_RESPONSE_VALUE => continue,
            _ => encode(p.id, 0, ""),
        };
        if sock.write_all(&reply).await.is_err() {
            return;
        }
    }
}
