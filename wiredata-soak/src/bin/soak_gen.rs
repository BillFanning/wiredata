//! Send sequence-numbered 64-byte datagrams for a listener soak run
//! (listener ADR-051).
//!
//! ```text
//! soak-gen --to 127.0.0.1:20000 --streams 4 --rate 100 --seconds 259200 --manifest gen.txt
//! ```
//!
//! Stream `i` goes to the destination port plus `i`, at `--rate` datagrams a
//! second, numbered from 0. A sequence number advances only when its send
//! succeeds, so a refused send is not mistaken for one listener lost. The
//! manifest, which `soak-verify` reads, is rewritten every second and at the
//! end, so an interrupted run still says what it sent.

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wiredata_soak::Record;

struct Args {
    to: SocketAddr,
    streams: u16,
    rate: u64,
    seconds: u64,
    manifest: PathBuf,
}

const USAGE: &str = "usage: soak-gen --to HOST:PORT --streams N --rate PER_SECOND \
                     --seconds S --manifest FILE";

fn parse() -> Result<Args, String> {
    let mut to = None;
    let mut streams = 1;
    let mut rate = 100;
    let mut seconds = None;
    let mut manifest = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--to" => to = Some(value()?.parse().map_err(|e| format!("--to: {e}"))?),
            "--streams" => streams = value()?.parse().map_err(|e| format!("--streams: {e}"))?,
            "--rate" => rate = value()?.parse().map_err(|e| format!("--rate: {e}"))?,
            "--seconds" => seconds = Some(value()?.parse().map_err(|e| format!("--seconds: {e}"))?),
            "--manifest" => manifest = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        to: to.ok_or("--to is required")?,
        streams,
        rate,
        seconds: seconds.ok_or("--seconds is required")?,
        manifest: manifest.ok_or("--manifest is required")?,
    })
}

/// What has been sent so far, for `soak-verify`. Written beside the target
/// and renamed over it, so a reader never sees half a manifest.
fn write_manifest(args: &Args, sent: &[u64]) -> std::io::Result<()> {
    let mut text = String::from("# soak-gen manifest: stream, port, first and last sent\n");
    for (stream, &count) in sent.iter().enumerate() {
        if count == 0 {
            continue;
        }
        text.push_str(&format!(
            "stream {stream} port {} first 0 last {}\n",
            args.to.port() + stream as u16,
            count - 1
        ));
    }
    let tmp = args.manifest.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &args.manifest)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(why) => {
            eprintln!("{why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let bind = if args.to.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = match UdpSocket::bind(bind) {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("cannot open a UDP socket: {e}");
            return ExitCode::from(1);
        }
    };
    // Harmless for unicast; needed when the destination is a broadcast address.
    let _ = socket.set_broadcast(true);
    let _resolution = wiredata_timing::high_resolution();

    let mut sent = vec![0u64; usize::from(args.streams)];
    let mut refused = 0u64;
    let started = Instant::now();
    let end = started + Duration::from_secs(args.seconds);
    let mut next_manifest = started;
    let mut next_progress = started + Duration::from_secs(10);
    loop {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(started);
        let due = (elapsed.as_secs_f64() * args.rate as f64) as u64;
        for (stream, count) in sent.iter_mut().enumerate() {
            let mut to = args.to;
            to.set_port(args.to.port() + stream as u16);
            while *count < due {
                let record = Record {
                    stream: stream as u16,
                    seq: *count,
                };
                match socket.send_to(&record.encode(), to) {
                    Ok(_) => *count += 1,
                    Err(e) => {
                        if refused == 0 {
                            eprintln!("a send was refused, retrying it next tick: {e}");
                        }
                        refused += 1;
                        break;
                    }
                }
            }
        }
        if now >= next_manifest {
            if let Err(e) = write_manifest(&args, &sent) {
                eprintln!("cannot write the manifest: {e}");
            }
            next_manifest += Duration::from_secs(1);
        }
        if now >= next_progress {
            println!(
                "{} s: {} sent per stream, {refused} sends refused and retried",
                elapsed.as_secs(),
                sent.iter().min().copied().unwrap_or(0)
            );
            next_progress += Duration::from_secs(10);
        }
        if now >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if let Err(e) = write_manifest(&args, &sent) {
        eprintln!("cannot write the manifest: {e}");
        return ExitCode::from(1);
    }
    println!(
        "done: {} streams, {} datagrams each, {refused} sends refused and retried",
        args.streams,
        sent.iter().min().copied().unwrap_or(0)
    );
    ExitCode::SUCCESS
}
