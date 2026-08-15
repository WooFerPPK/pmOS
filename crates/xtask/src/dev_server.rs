//! xtask dev-server — T027.
//!
//! Serves `dist/` over HTTP on a configurable port with the COOP/COEP
//! headers required for SharedArrayBuffer. Deliberately tiny: a
//! dependency-free single-threaded server is enough for development.
//!
//! Why not `basic-http-server`, `miniserve`, etc.? Because pulling a
//! dozen crates just to serve a static tree is exactly the kind of
//! JS-ecosystem bloat the project constitution argues against. This
//! is ~120 lines of std-only Rust.

use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn run(args: &[String]) -> std::result::Result<(), String> {
    run_inner(args).map_err(|e| format!("dev-server: {e}"))
}

fn run_inner(args: &[String]) -> Result<()> {
    let mut dir = PathBuf::from("dist");
    let mut port: u16 = 8080;

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--dir=") {
            dir = PathBuf::from(rest);
        } else if a == "--dir" {
            i += 1;
            dir = PathBuf::from(&args[i]);
        } else if let Some(rest) = a.strip_prefix("--port=") {
            port = rest.parse::<u16>().map_err(|e| format!("bad port: {e}"))?;
        } else if a == "--port" {
            i += 1;
            port = args[i]
                .parse::<u16>()
                .map_err(|e| format!("bad port: {e}"))?;
        } else {
            return Err(format!("unknown arg: {a}").into());
        }
        i += 1;
    }

    let dir = fs::canonicalize(&dir)
        .map_err(|e| format!("cannot resolve --dir {}: {e}", dir.display()))?;
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)?;

    println!(
        "[xtask] dev-server: serving {} on http://{}",
        dir.display(),
        addr
    );
    println!("[xtask] dev-server: COOP: same-origin, COEP: require-corp");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let d = dir.clone();
                thread::spawn(move || {
                    if let Err(e) = handle(s, &d) {
                        eprintln!("[xtask] dev-server: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[xtask] dev-server: accept failed: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, root: &Path) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let path = parse_path(&request_line).unwrap_or_else(|| "/".to_string());

    // Drain headers.
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let (status, body, content_type) = resolve(root, &path);
    let headers = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cross-Origin-Opener-Policy: same-origin\r\n\
         Cross-Origin-Embedder-Policy: require-corp\r\n\
         Cross-Origin-Resource-Policy: same-origin\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        status = status,
        content_type = content_type,
        len = body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

fn parse_path(request_line: &str) -> Option<String> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    Some(target.split('?').next().unwrap_or(target).to_string())
}

fn resolve(root: &Path, path: &str) -> (&'static str, Vec<u8>, &'static str) {
    // Normalise — forbid .. segments.
    let rel = path.trim_start_matches('/');
    if rel.split('/').any(|seg| seg == "..") {
        return ("400 Bad Request", b"bad path".to_vec(), "text/plain");
    }
    let mut resolved = root.join(rel);
    if resolved.is_dir() {
        resolved = resolved.join("index.html");
    }
    if rel.is_empty() {
        resolved = root.join("index.html");
    }
    match fs::read(&resolved) {
        Ok(body) => {
            let ct = mime_for(&resolved);
            ("200 OK", body, ct)
        }
        Err(_) => ("404 Not Found", b"not found".to_vec(), "text/plain"),
    }
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("toml") => "text/plain; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[allow(dead_code)]
fn _read_fully(mut r: impl Read) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    r.read_to_end(&mut v)?;
    Ok(v)
}
