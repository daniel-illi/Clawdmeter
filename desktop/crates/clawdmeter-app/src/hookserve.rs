//! Hook ingest over HTTP: `POST /hook?mode=<mode>` on loopback.
//!
//! The request body is the hook JSON Claude Code pipes to hook commands;
//! it goes to [`crate::hooks::run`] unchanged, so a POST writes exactly the
//! files `clawdmeter hook` writes (events.log, attention, clear-<sid>).
//! Bound to 127.0.0.1 only: sandboxed agents reach it through the sbx
//! egress proxy, which connects from the host. Anything it can write a
//! local process could already write into the state dir.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::hooks::{self, Mode};

/// How far into a request to look for the header/body split. The check
/// runs after searching each read, so the effective limit is a read
/// chunk past this.
const HEAD_CAP: usize = 8 * 1024;
/// Hook payloads are usually a few KB, but a pasted prompt or a verbose
/// tool_response can be large — the CLI hook path has no cap at all. Keep
/// the cap generous so real events are not dropped.
const BODY_CAP: usize = 1024 * 1024;
/// A stuck client must not hold a task forever.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

/// A live listener: the bound port and its accept task.
pub struct Listener {
    /// The port it listens on (rebind key).
    pub port: u16,
    task: JoinHandle<()>,
}

/// Bind `port` on loopback and serve until the task is aborted. `0`
/// disables the listener; a failed bind yields `Err` — the caller decides
/// how to report it.
pub async fn bind(port: u16, state_dir: PathBuf) -> io::Result<Option<Listener>> {
    if port == 0 {
        return Ok(None);
    }
    let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    log::info!("hook listener on http://127.0.0.1:{port}/hook");
    Ok(Some(Listener { port, task: tokio::spawn(serve(l, state_dir)) }))
}

/// Keep the listener on the configured port: rebind when it changed.
/// Aborting the accept task drops the socket; connections already accepted
/// finish on their own (a few hundred ms at most).
pub async fn rebind(listener: &mut Option<Listener>, port: u16, state_dir: &Path) -> io::Result<()> {
    if listener.as_ref().is_some_and(|l| l.port == port) {
        return Ok(());
    }
    if let Some(l) = listener.take() {
        l.task.abort();
    }
    *listener = bind(port, state_dir.to_path_buf()).await?;
    Ok(())
}

/// Max simultaneous connections being handled — bounds the fds and tasks
/// a connection flood can take from the app.
const MAX_CONNS: usize = 64;

/// Accept until aborted; each connection is its own task.
async fn serve(listener: TcpListener, state_dir: PathBuf) {
    let permits = std::sync::Arc::new(Semaphore::new(MAX_CONNS));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue; // saturated: close the socket without reading
                };
                let dir = state_dir.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(CONN_TIMEOUT, handle(stream, dir)).await;
                });
            }
            Err(e) => {
                log::warn!("hook listener accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// One request: read the head, then the body, run the hook, answer.
async fn handle(mut stream: TcpStream, dir: PathBuf) -> io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let split = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() >= HEAD_CAP {
            return refuse(&mut stream, 431).await;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // client went away mid-request
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    buf.drain(..split + 4);

    let mut lines = head.lines();
    let mut req = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (req.next().unwrap_or(""), req.next().unwrap_or(""));
    if method != "POST" {
        return refuse(&mut stream, 405).await;
    }
    // A proxy may forward the absolute URI verbatim: strip the authority.
    let target = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
        .map_or(target, |rest| rest.find('/').map_or("/", |i| &rest[i..]));
    let (path, query) = target.split_once('?').map_or((target, ""), |(p, q)| (p, q));
    if path != "/hook" {
        return refuse(&mut stream, 404).await;
    }
    let Some(len) = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
    else {
        // e.g. Transfer-Encoding: chunked — we don't read framed bodies,
        // and silently dropping the event is worse than refusing it.
        return refuse(&mut stream, 411).await;
    };
    if len > BODY_CAP {
        return refuse(&mut stream, 413).await;
    }
    while buf.len() < len {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let mut body = String::from_utf8_lossy(&buf[..len]).into_owned();
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // HTTP posts come from sandboxed/remote agents: their transcript_path
    // is a path in *their* filesystem, and a collision with a host
    // config_dirs entry would wrongly attribute the event — including
    // letting the probe start a 5-hour window on the wrong machine. Local
    // sessions use `clawdmeter hook`, which keeps attribution.
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(o) = v.as_object_mut() {
            if o.remove("transcript_path").is_some() {
                body = v.to_string();
            }
        }
    }
    let mode = mode_of(query);
    // Blocking file I/O and up to a 1 MiB JSON parse stay off the
    // single-threaded runtime.
    let _ = tokio::task::spawn_blocking(move || hooks::run(mode, &body, &dir, now)).await;
    respond(&mut stream, 204).await
}

/// The `mode` query parameter as hook arguments: words joined by `+` or
/// whitespace (`?mode=clear+event`), parsed like `clawdmeter hook`'s argv.
fn mode_of(query: &str) -> Mode {
    let raw = query.split('&').find_map(|p| p.strip_prefix("mode=")).unwrap_or("");
    let words: Vec<String> = raw.split(['+', ' ']).filter(|s| !s.is_empty()).map(str::to_owned).collect();
    Mode::parse(&words)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Answer an error, then drain briefly: closing with the request body
/// still unread makes the kernel RST, which can eat the response before
/// the client reads it.
async fn refuse(stream: &mut TcpStream, code: u16) -> io::Result<()> {
    respond(stream, code).await?;
    let _ = stream.shutdown().await;
    let mut sink = [0u8; 2048];
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        while stream.read(&mut sink).await.unwrap_or(0) != 0 {}
    })
    .await;
    Ok(())
}

async fn respond(stream: &mut TcpStream, code: u16) -> io::Result<()> {
    let text = match code {
        204 => "204 No Content",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        411 => "411 Length Required",
        413 => "413 Content Too Large",
        431 => "431 Request Header Fields Too Large",
        _ => "400 Bad Request",
    };
    stream.write_all(format!("HTTP/1.1 {text}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cm-hookserve-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Send `req`, read the whole response (the server closes after it).
    async fn exchange(port: u16, req: &str) -> String {
        let mut c = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn modes() {
        assert_eq!(mode_of("mode=event"), Mode { action: hooks::Action::Event, log_too: false });
        assert_eq!(mode_of("mode=clear+event"), Mode { action: hooks::Action::Clear, log_too: true });
        assert_eq!(mode_of("mode=alert%20event"), Mode { action: hooks::Action::None, log_too: false });
        assert_eq!(mode_of("mode=bogus+event"), Mode { action: hooks::Action::None, log_too: true });
        assert_eq!(mode_of(""), Mode { action: hooks::Action::None, log_too: false });
        assert_eq!(mode_of("x=1&mode=event"), Mode { action: hooks::Action::Event, log_too: false });
    }

    #[tokio::test]
    async fn posts_write_the_hook_files() {
        let d = dir("posts");
        let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, d.clone()));

        let body = r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1"}"#;
        let req = format!("POST /hook?mode=clear+event HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        assert!(exchange(port, &req).await.starts_with("HTTP/1.1 204"));
        let log = fs::read_to_string(d.join("events.log")).unwrap();
        assert!(log.trim_end().ends_with(" UserPromptSubmit s1"), "{log}");
        assert!(d.join("clear-s1").is_file());

        let body = r#"{"hook_event_name":"Notification","message":"Perm?","session_id":"s1"}"#;
        let req = format!("POST /hook?mode=alert+event HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        assert!(exchange(port, &req).await.starts_with("HTTP/1.1 204"));
        assert_eq!(
            fs::read_to_string(d.join("attention")).unwrap(),
            "{\"k\":\"permission\",\"m\":\"Perm?\",\"s\":\"s1\"}\n"
        );

        assert!(exchange(port, "GET /hook?mode=event HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 405"));
        assert!(exchange(port, "POST /other HTTP/1.1\r\nContent-Length: 0\r\n\r\n").await.starts_with("HTTP/1.1 404"));
        assert!(exchange(port, "POST /hook?mode=event HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 411"));
        let big = format!("POST /hook?mode=event HTTP/1.1\r\nContent-Length: {}\r\n\r\n", BODY_CAP + 1);
        assert!(exchange(port, &big).await.starts_with("HTTP/1.1 413"));

        // No mode at all still answers but writes nothing.
        let req = format!("POST /hook HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        assert!(exchange(port, &req).await.starts_with("HTTP/1.1 204"));
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn sandbox_posts_stay_unattributed() {
        let d = dir("unattributed");
        let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, d.clone()));

        // A transcript_path from the sandbox filesystem must not become an
        // account column — it could collide with a host config_dirs entry.
        let body = r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","transcript_path":"/home/agent/.claude/projects/p/s.jsonl"}"#;
        let req = format!("POST /hook?mode=event HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
        assert!(exchange(port, &req).await.starts_with("HTTP/1.1 204"));
        let line = fs::read_to_string(d.join("events.log")).unwrap();
        assert_eq!(line.trim_end().split(' ').count(), 3, "ts event session, no account: {line}");
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn absolute_form_and_partial_requests() {
        let d = dir("absform");
        let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, d.clone()));

        // Absolute-form target (what a proxy may forward verbatim).
        let body = r#"{"hook_event_name":"Stop","session_id":"a1"}"#;
        let req = format!(
            "POST http://host.docker.internal:{port}/hook?mode=event HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        assert!(exchange(port, &req).await.starts_with("HTTP/1.1 204"));
        assert!(fs::read_to_string(d.join("events.log")).unwrap().contains("Stop a1"));

        // Head over the cap is refused.
        let huge_head = format!("POST /hook HTTP/1.1\r\nX: {}\r\n\r\n", "y".repeat(HEAD_CAP));
        assert!(exchange(port, &huge_head).await.starts_with("HTTP/1.1 431"));

        // Body cut short: connection closes with no response and no write.
        let mut c = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        c.write_all(b"POST /hook?mode=event HTTP/1.1\r\nContent-Length: 100\r\n\r\n{}").await.unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out).await; // FIN or RST — never a response
        assert!(out.is_empty());
        assert!(fs::read_to_string(d.join("events.log")).unwrap().lines().count() == 1);
        let _ = fs::remove_dir_all(&d);
    }

    /// A port we know is free, by borrowing it briefly.
    async fn free_port() -> u16 {
        let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn bind_and_rebind_follow_the_port() {
        let d = dir("rebind");
        let mut l = bind(0, d.clone()).await.unwrap();
        assert!(l.is_none(), "hook_port = 0 must not bind");

        let p1 = free_port().await;
        rebind(&mut l, p1, &d).await.unwrap();
        assert_eq!(l.as_ref().map(|x| x.port), Some(p1));
        let body = r#"{"hook_event_name":"Stop","session_id":"r1"}"#;
        let req = format!("POST /hook?mode=event HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        assert!(exchange(p1, &req).await.starts_with("HTTP/1.1 204"));

        // Same port again: no rebind, still serving.
        rebind(&mut l, p1, &d).await.unwrap();

        let p2 = free_port().await;
        rebind(&mut l, p2, &d).await.unwrap();
        assert_eq!(l.as_ref().map(|x| x.port), Some(p2));
        assert!(TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, p1)).await.is_err(), "old port closed");
        assert!(exchange(p2, &req).await.starts_with("HTTP/1.1 204"));

        rebind(&mut l, 0, &d).await.unwrap();
        assert!(l.is_none());
        assert!(TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, p2)).await.is_err());
        assert!(fs::read_to_string(d.join("events.log")).unwrap().lines().count() == 2);
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn failed_rebind_leaves_no_listener() {
        let d = dir("failrebind");
        // Occupy a port so the bind must fail.
        let held = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = held.local_addr().unwrap().port();
        let mut l = bind(0, d.clone()).await.unwrap();
        assert!(rebind(&mut l, port, &d).await.is_err());
        assert!(l.is_none());
        let _ = fs::remove_dir_all(&d);
    }
}
