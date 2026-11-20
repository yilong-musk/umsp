//! Concurrency load generator for the load test (spec 8.4). Not part of the
//! release binary; it speaks HTTP/1.1 over `std::net` so the test needs no
//! extra crates.
//!
//! ```text
//! cargo run --release --example loadgen -- \
//!     --addr 127.0.0.1:19997 --path /v1/chat/completions \
//!     --key umsp_... --model od/load-model \
//!     --concurrency 50 --duration 30
//! ```
//!
//! `--requests N` runs a fixed number instead of a duration (used for the
//! 10,000-request resident-memory check). `--stream` asks for SSE and measures
//! the time to the first byte after the response headers.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Cfg {
    addr: SocketAddr,
    host: String,
    path: String,
    key: String,
    model: String,
    stream: bool,
    concurrency: usize,
    duration: Option<Duration>,
    requests: Option<u64>,
}

#[derive(Default)]
struct Stats {
    stop: AtomicBool,
    issued: AtomicU64,
    samples: Mutex<Vec<Sample>>,
    errors: Mutex<BTreeMap<String, u64>>,
}

#[derive(Clone)]
struct Sample {
    status: u16,
    ttfb_ms: f64,
    total_ms: f64,
}

fn main() {
    let mut cfg = Cfg {
        addr: "127.0.0.1:19997".parse().unwrap(),
        host: "127.0.0.1:19997".to_string(),
        path: "/v1/chat/completions".to_string(),
        key: String::new(),
        model: "od/load-model".to_string(),
        stream: false,
        concurrency: 1,
        duration: Some(Duration::from_secs(30)),
        requests: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().expect("missing value");
        match arg.as_str() {
            "--addr" => {
                let raw = value();
                cfg.addr = raw
                    .to_socket_addrs()
                    .expect("bad --addr")
                    .next()
                    .expect("bad --addr");
                cfg.host = raw;
            }
            "--path" => cfg.path = value(),
            "--key" => cfg.key = value(),
            "--model" => cfg.model = value(),
            "--concurrency" => cfg.concurrency = value().parse().expect("bad --concurrency"),
            "--duration" => {
                cfg.duration = Some(Duration::from_secs(
                    value().parse().expect("bad --duration"),
                ))
            }
            "--requests" => {
                cfg.requests = Some(value().parse().expect("bad --requests"));
                cfg.duration = None;
            }
            "--stream" => cfg.stream = true,
            other => panic!("unknown argument: {other}"),
        }
    }
    assert!(!cfg.key.is_empty(), "--key is required");

    let stats = Arc::new(Stats::default());
    let started = Instant::now();
    let mut workers = Vec::new();
    for _ in 0..cfg.concurrency {
        let cfg = cfg.clone();
        let stats = stats.clone();
        workers.push(std::thread::spawn(move || worker(&cfg, &stats)));
    }
    if let Some(duration) = cfg.duration {
        std::thread::sleep(duration);
        stats.stop.store(true, Ordering::SeqCst);
    }
    for worker in workers {
        let _ = worker.join();
    }
    let wall = started.elapsed().as_secs_f64();

    let samples = stats.samples.lock().unwrap();
    let errors = stats.errors.lock().unwrap();
    let mut statuses: BTreeMap<String, u64> = BTreeMap::new();
    for sample in samples.iter() {
        *statuses.entry(sample.status.to_string()).or_default() += 1;
    }
    let ok = samples
        .iter()
        .filter(|sample| (200..300).contains(&sample.status))
        .count();
    let mut ttfb: Vec<f64> = samples.iter().map(|s| s.ttfb_ms).collect();
    let mut total: Vec<f64> = samples.iter().map(|s| s.total_ms).collect();
    let summary = serde_json::json!({
        "requests": samples.len(),
        "ok": ok,
        "failed": samples.len() - ok,
        "concurrency": cfg.concurrency,
        "stream": cfg.stream,
        "wall_s": wall,
        "rps": (samples.len() as f64 / wall * 100.0).round() / 100.0,
        "status": statuses,
        "errors": errors.clone(),
        "ttfb_ms": percentiles(&mut ttfb),
        "total_ms": percentiles(&mut total),
    });
    println!("{summary}");
}

fn worker(cfg: &Cfg, stats: &Stats) {
    loop {
        if stats.stop.load(Ordering::Relaxed) {
            return;
        }
        if let Some(limit) = cfg.requests
            && stats.issued.fetch_add(1, Ordering::SeqCst) >= limit
        {
            return;
        }
        match one_request(cfg) {
            Ok(sample) => stats.samples.lock().unwrap().push(sample),
            Err(kind) => {
                stats.stop.store(true, Ordering::SeqCst);
                *stats.errors.lock().unwrap().entry(kind).or_default() += 1;
            }
        }
    }
}

fn one_request(cfg: &Cfg) -> Result<Sample, String> {
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [{"role": "user", "content": "load test"}],
        "stream": cfg.stream,
    })
    .to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {key}\r\n\
         Content-Type: application/json\r\nContent-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        path = cfg.path,
        host = cfg.host,
        key = cfg.key,
        len = body.len(),
    );
    let mut stream = TcpStream::connect_timeout(&cfg.addr, Duration::from_secs(5))
        .map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| format!("read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("write timeout: {e}"))?;

    let started = Instant::now();
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut raw: Vec<u8> = Vec::with_capacity(8192);
    let mut header_end: Option<usize> = None;
    let mut ttfb_ms: Option<f64> = None;
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if read == 0 {
            break;
        }
        let now_ms = started.elapsed().as_secs_f64() * 1000.0;
        raw.extend_from_slice(&chunk[..read]);
        if header_end.is_none()
            && let Some(position) = find(&raw, b"\r\n\r\n")
        {
            header_end = Some(position);
            if raw.len() > position + 4 {
                ttfb_ms = Some(now_ms);
            }
        }
        if ttfb_ms.is_none() && header_end.is_some() {
            ttfb_ms = Some(now_ms);
        }
        if raw.len() > 8 * 1024 * 1024 {
            return Err("response larger than 8MiB".to_string());
        }
    }
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    let Some(header_end) = header_end else {
        return Err("no response headers".to_string());
    };
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "unparsable status line".to_string())?;
    Ok(Sample {
        status,
        ttfb_ms: ttfb_ms.unwrap_or(total_ms),
        total_ms,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn percentiles(values: &mut [f64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::json!({"min": 0, "avg": 0, "p50": 0, "p95": 0, "p99": 0, "max": 0});
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| {
        let index = ((values.len() - 1) as f64 * q).round() as usize;
        (values[index] * 100.0).round() / 100.0
    };
    let sum: f64 = values.iter().sum();
    serde_json::json!({
        "min": (values[0] * 100.0).round() / 100.0,
        "avg": (sum / values.len() as f64 * 100.0).round() / 100.0,
        "p50": at(0.50),
        "p95": at(0.95),
        "p99": at(0.99),
        "max": (values[values.len() - 1] * 100.0).round() / 100.0,
    })
}
