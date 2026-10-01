//! Check listener's recordings of a soak run against what `soak-gen` sent
//! (listener ADR-051).
//!
//! ```text
//! soak-verify --manifest gen.txt --recordings DIR --logs LOGDIR ch00=0 ch01=1 ...
//! ```
//!
//! Each `CHANNEL=STREAM` names a listener Channel and the generator stream it
//! received. Its `.raw` files are those in `--recordings` named `CHANNEL.raw`
//! or `CHANNEL_<digit>….raw`. Its gaps come from the event log files in
//! `--logs`. Exits 0 when every Channel passes, 1 when any fails, 2 on a usage
//! or read error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wiredata_soak::{verify, LoggedGaps, Segment};

/// How many findings to list for a failing Channel before summarizing.
const LISTED: usize = 20;

struct Args {
    manifest: PathBuf,
    recordings: PathBuf,
    logs: PathBuf,
    channels: Vec<(String, u16)>,
}

const USAGE: &str =
    "usage: soak-verify --manifest FILE --recordings DIR --logs DIR CHANNEL=STREAM...";

fn parse() -> Result<Args, String> {
    let (mut manifest, mut recordings, mut logs) = (None, None, None);
    let mut channels = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--manifest" => manifest = Some(PathBuf::from(value()?)),
            "--recordings" => recordings = Some(PathBuf::from(value()?)),
            "--logs" => logs = Some(PathBuf::from(value()?)),
            pair => {
                let (name, stream) = pair
                    .split_once('=')
                    .ok_or(format!("expected CHANNEL=STREAM, not {pair}"))?;
                let stream = stream.parse().map_err(|e| format!("{pair}: stream: {e}"))?;
                channels.push((name.to_owned(), stream));
            }
        }
    }
    if channels.is_empty() {
        return Err("name at least one CHANNEL=STREAM".to_owned());
    }
    Ok(Args {
        manifest: manifest.ok_or("--manifest is required")?,
        recordings: recordings.ok_or("--recordings is required")?,
        logs: logs.ok_or("--logs is required")?,
        channels,
    })
}

/// The last sequence number sent on each stream, from the manifest.
fn read_manifest(path: &Path) -> Result<HashMap<u16, (u64, u64)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut streams = HashMap::new();
    for line in text.lines().filter(|l| l.starts_with("stream ")) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let field = |name: &str| {
            words
                .iter()
                .position(|w| *w == name)
                .and_then(|i| words.get(i + 1))
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or(format!("manifest line without {name}: {line}"))
        };
        let stream = u16::try_from(field("stream")?).map_err(|e| e.to_string())?;
        streams.insert(stream, (field("first")?, field("last")?));
    }
    Ok(streams)
}

/// Whether `file` is one of `channel`'s recordings: `CHANNEL.raw`, or
/// `CHANNEL_` then a period or segment number, which starts with a digit.
fn belongs_to(file: &str, channel: &str) -> bool {
    let Some(stem) = file.strip_suffix(".raw") else {
        return false;
    };
    stem == channel
        || stem
            .strip_prefix(channel)
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

fn read_logs(dir: &Path) -> Result<String, String> {
    let mut text = String::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("listener.log")
        {
            let bytes = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
            text.push_str(&String::from_utf8_lossy(&bytes));
            text.push('\n');
        }
    }
    Ok(text)
}

fn run(args: &Args) -> Result<bool, String> {
    let manifest = read_manifest(&args.manifest)?;
    let logs = read_logs(&args.logs)?;
    let files: Vec<PathBuf> = std::fs::read_dir(&args.recordings)
        .map_err(|e| format!("{}: {e}", args.recordings.display()))?
        .flatten()
        .map(|entry| entry.path())
        .collect();

    let mut all_passed = true;
    for (channel, stream) in &args.channels {
        let &(first, last) = manifest
            .get(stream)
            .ok_or(format!("the manifest has no stream {stream}"))?;
        let mut segments = Vec::new();
        for path in &files {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if belongs_to(&name, channel) {
                let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
                segments.push(Segment::read(name.into_owned(), &bytes));
            }
        }
        let files = segments.len();
        let gaps = LoggedGaps::read(channel, &logs);
        let verdict = verify(*stream, first, last, segments, &gaps);
        if verdict.passed() {
            println!(
                "{channel}: PASS: {} records in {files} files, {} excused by {} logged gaps",
                verdict.records,
                verdict.excused,
                gaps.resumed_in.len() + usize::from(gaps.stopped_in_gap)
            );
        } else {
            all_passed = false;
            println!(
                "{channel}: FAIL: {} findings, {} records in {files} files",
                verdict.findings.len(),
                verdict.records
            );
            for finding in verdict.findings.iter().take(LISTED) {
                println!("  {finding}");
            }
            if verdict.findings.len() > LISTED {
                println!("  … and {} more", verdict.findings.len() - LISTED);
            }
        }
    }
    Ok(all_passed)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(why) => {
            eprintln!("{why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(why) => {
            eprintln!("{why}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_s_files_are_told_apart_from_a_longer_name_s() {
        assert!(belongs_to("ch01.raw", "ch01"));
        assert!(belongs_to("ch01_2026-10-01_08.raw", "ch01"));
        assert!(belongs_to("ch01_2026-10-01_08_2.raw", "ch01"));
        assert!(!belongs_to("ch01_B_2026-10-01.raw", "ch01"));
        assert!(!belongs_to("ch010_2026-10-01.raw", "ch01"));
        assert!(!belongs_to("ch01_2026-10-01_08.disp", "ch01"));
        assert!(!belongs_to("ch01_2026-10-01_08.raw.idx", "ch01"));
    }
}
