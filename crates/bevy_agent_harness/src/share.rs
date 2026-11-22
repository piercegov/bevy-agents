//! A tiny HTTP server that shows one video (or image) in a browser:
//! `bevy-agent share demo.mp4` and `bevy-agent record stop --share`.
//!
//! It serves a player page at `/` and the file itself, with HTTP Range support
//! (browsers refuse to play video without it), and exits once nobody has asked
//! for anything for a while.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

/// Default for `--bind`: an IP address, or `tailscale`.
pub const BIND_ENV: &str = "BEVY_AGENT_SHARE_BIND";

/// Resolve a `--bind` value. `tailscale` means this machine's Tailscale IPv4
/// address, so other devices on the tailnet (and nothing else) can connect.
pub fn resolve_bind(bind: &str) -> Result<IpAddr> {
    let bind = bind.trim();
    if bind.eq_ignore_ascii_case("tailscale") {
        let output = std::process::Command::new("tailscale")
            .args(["ip", "-4"])
            .output()
            .context("`--bind tailscale` needs the tailscale command")?;
        let text = String::from_utf8_lossy(&output.stdout);
        return text
            .lines()
            .find_map(|l| l.trim().parse().ok())
            .ok_or_else(|| anyhow!("`tailscale ip -4` printed no address; is Tailscale up?"));
    }
    if bind.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::from([127, 0, 0, 1]));
    }
    bind.parse()
        .map_err(|_| anyhow!("`{bind}` is not an IP address; use one, or `tailscale`"))
}

fn media(path: &Path) -> Result<(&'static str, &'static str, bool)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // (extension, content type, is a video)
    Ok(match ext.as_str() {
        "mp4" | "m4v" => ("mp4", "video/mp4", true),
        "mov" => ("mov", "video/quicktime", true),
        "mkv" => ("mkv", "video/x-matroska", true),
        "webm" => ("webm", "video/webm", true),
        "gif" => ("gif", "image/gif", false),
        "png" => ("png", "image/png", false),
        "jpg" | "jpeg" => ("jpg", "image/jpeg", false),
        other => bail!("cannot share `.{other}` files: use mp4, mov, mkv, webm, gif, png or jpg"),
    })
}

/// Check that `path` can be shared, before starting a server for it.
pub fn check(path: &Path) -> Result<()> {
    media(path)?;
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    Ok(())
}

/// A port on `ip` that is free right now.
pub fn free_port(ip: IpAddr) -> Result<u16> {
    let listener =
        TcpListener::bind(SocketAddr::new(ip, 0)).with_context(|| format!("binding to {ip}"))?;
    Ok(listener.local_addr()?.port())
}

/// The address to give a person for a server bound to `ip`.
pub fn url(ip: IpAddr, port: u16) -> String {
    format!("http://{}/", SocketAddr::new(ip, port))
}

/// Whether something accepts connections on `ip:port`.
pub fn is_up(ip: IpAddr, port: u16) -> bool {
    let ip = if ip.is_unspecified() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        ip
    };
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_millis(300)).is_ok()
}

struct Shared {
    path: PathBuf,
    file_route: String,
    content_type: &'static str,
    page: String,
    started: Instant,
    /// Milliseconds after `started` of the latest request.
    last_request: AtomicU64,
}

/// Serve `path` until no request has arrived for `idle`. Blocks.
pub fn serve(path: &Path, ip: IpAddr, port: u16, idle: Duration) -> Result<()> {
    check(path)?;
    let (ext, content_type, is_video) = media(path)?;
    let listener = TcpListener::bind(SocketAddr::new(ip, port))
        .with_context(|| format!("binding to {ip}:{port}"))?;
    listener.set_nonblocking(true)?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A fixed route, so the file's real name never has to be URL-encoded.
    let file_route = format!("/media.{ext}");
    let shared = Arc::new(Shared {
        path: path.to_path_buf(),
        page: page(&escape(&name), &file_route, is_video),
        file_route,
        content_type,
        started: Instant::now(),
        last_request: AtomicU64::new(0),
    });

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = shared.clone();
                let now = shared.started.elapsed().as_millis() as u64;
                shared.last_request.store(now, Ordering::Relaxed);
                std::thread::spawn(move || {
                    let _ = handle(stream, &shared);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let last = Duration::from_millis(shared.last_request.load(Ordering::Relaxed));
                if shared.started.elapsed().saturating_sub(last) > idle {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, file_route: &str, is_video: bool) -> String {
    let src = file_route.trim_start_matches('/');
    let element = if is_video {
        format!(r#"<video src="{src}" controls autoplay loop muted playsinline></video>"#)
    } else {
        format!(r#"<img src="{src}" alt="{title}">"#)
    };
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
  body {{ margin: 0; background: #111; color: #bbb; font: 13px system-ui, sans-serif;
         display: flex; flex-direction: column; align-items: center; justify-content: center;
         min-height: 100vh; gap: 8px; }}
  video, img {{ max-width: 100vw; max-height: 92vh; }}
</style>
{element}
<div>{title}</div>
"#
    )
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra: &str,
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    )?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

/// Parse a `Range: bytes=a-b` value into an inclusive byte range of a file of `size` bytes.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    let (start, end) = if first.is_empty() {
        // The last N bytes.
        let n: u64 = last.parse().ok()?;
        (size.saturating_sub(n), size.checked_sub(1)?)
    } else {
        let start: u64 = first.parse().ok()?;
        let end = match last {
            "" => size.checked_sub(1)?,
            last => last.parse::<u64>().ok()?.min(size.checked_sub(1)?),
        };
        (start, end)
    };
    (start <= end && start < size).then_some((start, end))
}

fn handle(mut stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_string());
        }
    }

    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            "",
            b"",
            false,
        );
    }
    let route = target.split(['?', '#']).next().unwrap_or("");
    if route == "/" || route == "/index.html" {
        let html = "text/html; charset=utf-8";
        return respond(
            &mut stream,
            "200 OK",
            html,
            "",
            shared.page.as_bytes(),
            head_only,
        );
    }
    if route != shared.file_route {
        return respond(
            &mut stream,
            "404 Not Found",
            "text/plain",
            "",
            b"not found\n",
            head_only,
        );
    }

    let mut file = File::open(&shared.path)?;
    let size = file.metadata()?.len();
    let (status, start, end) = match range {
        None => ("200 OK", 0, size.saturating_sub(1)),
        Some(value) => match parse_range(&value, size) {
            Some((start, end)) => ("206 Partial Content", start, end),
            None => {
                let extra = format!("Content-Range: bytes */{size}\r\n");
                return respond(
                    &mut stream,
                    "416 Range Not Satisfiable",
                    "text/plain",
                    &extra,
                    b"",
                    head_only,
                );
            }
        },
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {length}\r\n\
         Accept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n",
        shared.content_type
    )?;
    if status.starts_with("206") {
        write!(stream, "Content-Range: bytes {start}-{end}/{size}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    if !head_only && length > 0 {
        file.seek(SeekFrom::Start(start))?;
        std::io::copy(&mut file.take(length), &mut stream)?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=990-5000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-2", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
    }

    #[test]
    fn binds() {
        assert_eq!(
            resolve_bind("127.0.0.1").unwrap(),
            IpAddr::from([127, 0, 0, 1])
        );
        assert!(resolve_bind("my-desktop").is_err());
        assert_eq!(
            url(IpAddr::from([10, 0, 0, 2]), 8080),
            "http://10.0.0.2:8080/"
        );
    }

    #[test]
    fn serves_page_and_ranges() {
        let dir = std::env::temp_dir().join(format!("bevy_agent_share_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a <clip>.mp4");
        std::fs::write(&path, (0..=255u8).collect::<Vec<_>>()).unwrap();
        let ip = IpAddr::from([127, 0, 0, 1]);
        let port = free_port(ip).unwrap();
        let served = path.clone();
        std::thread::spawn(move || serve(&served, ip, port, Duration::from_secs(30)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !is_up(ip, port) {
            assert!(Instant::now() < deadline, "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }

        let get = |target: &str, range: Option<&str>| -> Vec<u8> {
            let mut stream = TcpStream::connect((ip, port)).unwrap();
            let range = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
            write!(stream, "GET {target} HTTP/1.1\r\nHost: x\r\n{range}\r\n").unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            response
        };
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();

        let page = text(&get("/", None));
        assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
        assert!(page.contains(r#"<video src="media.mp4""#), "{page}");
        assert!(page.contains("a &lt;clip&gt;.mp4"), "{page}");

        let part = get("/media.mp4", Some("bytes=10-13"));
        let head = text(&part);
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert!(head.contains("Content-Range: bytes 10-13/256"), "{head}");
        assert!(part.ends_with(&[10, 11, 12, 13]));

        let whole = get("/media.mp4", None);
        assert!(text(&whole).contains("Content-Length: 256"));
        assert!(text(&get("/../etc/passwd", None)).starts_with("HTTP/1.1 404"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
