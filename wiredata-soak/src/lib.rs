//! Soak harness for listener (listener ADR-051): a generator of
//! sequence-numbered UDP datagrams, and an offline verifier of what listener
//! recorded.
//!
//! The harness belongs to neither application. It shares no code with them,
//! so a fault in listener cannot hide in a shared reader: the verifier reads
//! `.raw` files and the event log as an operator would.
//!
//! **What the verifier holds listener to** (listener §56.1, §59):
//!
//! - A segment is contiguous: its records run one sequence number after
//!   another, with nothing missing, repeated or out of order.
//! - Between segments, sequence numbers may jump only where listener logged a
//!   gap: the later segment is one the event log says recording "resumed in".
//! - The run covers what the generator sent, from its first sequence number to
//!   its last, unless the recording began after a gap or stopped during one.
//! - Across the run, no sequence number appears twice and none goes backwards.

use std::fmt;

/// Every datagram is this long, so a segment is a whole number of records.
pub const RECORD_LEN: usize = 64;

/// The first bytes of every record.
pub const MAGIC: [u8; 4] = *b"WDSK";

/// One generated datagram: which stream it belongs to and its place in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub stream: u16,
    pub seq: u64,
}

impl Record {
    /// The 64 wire bytes: magic, stream, sequence number, a filler that
    /// depends on both, and a checksum over the rest. The filler makes a
    /// record whose bytes were altered fail its checksum rather than pass as
    /// another.
    pub fn encode(self) -> [u8; RECORD_LEN] {
        let mut bytes = [0u8; RECORD_LEN];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&self.stream.to_le_bytes());
        bytes[6..14].copy_from_slice(&self.seq.to_le_bytes());
        for (i, byte) in bytes[14..60].iter_mut().enumerate() {
            *byte = (self.seq as u8)
                .wrapping_add(self.stream as u8)
                .wrapping_add(i as u8);
        }
        let sum = fnv1a(&bytes[..60]);
        bytes[60..64].copy_from_slice(&sum.to_le_bytes());
        bytes
    }

    /// Read one record back, or say why these bytes are not one.
    pub fn decode(bytes: &[u8; RECORD_LEN]) -> Result<Self, &'static str> {
        if bytes[0..4] != MAGIC {
            return Err("no record marker");
        }
        let sum = u32::from_le_bytes(bytes[60..64].try_into().unwrap());
        if fnv1a(&bytes[..60]) != sum {
            return Err("checksum mismatch");
        }
        Ok(Self {
            stream: u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
            seq: u64::from_le_bytes(bytes[6..14].try_into().unwrap()),
        })
    }
}

/// FNV-1a, 32 bits: enough to tell a damaged record from an intact one.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, &byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// One recorded file, as the verifier read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The file's name, as the event log names it.
    pub name: String,
    pub records: Vec<Record>,
    /// Bytes that are not a whole, intact record: a torn end or damage.
    pub damaged: Vec<String>,
}

impl Segment {
    /// Read a segment's bytes into records.
    pub fn read(name: impl Into<String>, bytes: &[u8]) -> Self {
        let mut records = Vec::with_capacity(bytes.len() / RECORD_LEN);
        let mut damaged = Vec::new();
        let (chunks, tail) = bytes.as_chunks::<RECORD_LEN>();
        let tail = tail.len();
        for (i, chunk) in chunks.iter().enumerate() {
            match Record::decode(chunk) {
                Ok(record) => records.push(record),
                Err(why) => damaged.push(format!("bytes {}: {why}", i * RECORD_LEN)),
            }
        }
        if tail > 0 {
            damaged.push(format!(
                "{tail} trailing bytes are not a whole {RECORD_LEN}-byte record"
            ));
        }
        Self {
            name: name.into(),
            records,
            damaged,
        }
    }

    fn first(&self) -> Option<u64> {
        self.records.first().map(|r| r.seq)
    }
}

/// What the event log says about one channel's raw recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoggedGaps {
    /// Files recording resumed in after a gap.
    pub resumed_in: Vec<String>,
    /// The recording stopped while a gap was open.
    pub stopped_in_gap: bool,
}

impl LoggedGaps {
    /// Read one channel's gap lines out of event-log text. The phrases are
    /// listener's own (its recorder reports, listener §56.1).
    pub fn read(channel: &str, log: &str) -> Self {
        let resumed = format!("Raw recording on channel {channel} resumed in a new file → ");
        let stopped = format!("Raw recording on channel {channel} stopped during a gap");
        let mut gaps = Self::default();
        for line in log.lines() {
            if let Some(at) = line.find(&resumed) {
                let path = line[at + resumed.len()..].trim();
                // The line may carry fields after the message; the path ends
                // at the file's extension.
                let path = path.find(".raw").map_or(path, |end| &path[..end + 4]);
                gaps.resumed_in.push(file_name(path).to_owned());
            }
            if line.contains(&stopped) {
                gaps.stopped_in_gap = true;
            }
        }
        gaps
    }
}

/// The last path component, whichever separator the log used.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// One finding against the rules in this crate's documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finding {
    /// Sequence numbers no segment holds and no logged gap explains.
    Missing { from: u64, to: u64, at: String },
    /// A sequence number recorded more than once.
    Duplicate { seq: u64, at: String },
    /// A sequence number lower than one recorded before it.
    Reordered { seq: u64, after: u64, at: String },
    /// A record from another stream.
    WrongStream { stream: u16, at: String },
    /// Bytes that are not an intact record.
    Damaged { at: String, what: String },
    /// Nothing was recorded at all.
    Empty,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { from, to, at } if from == to => {
                write!(f, "missing {from}, {at}")
            }
            Self::Missing { from, to, at } => write!(f, "missing {from}–{to}, {at}"),
            Self::Duplicate { seq, at } => write!(f, "duplicate {seq} in {at}"),
            Self::Reordered { seq, after, at } => write!(f, "{seq} after {after} in {at}"),
            Self::WrongStream { stream, at } => write!(f, "a record of stream {stream} in {at}"),
            Self::Damaged { at, what } => write!(f, "{at}: {what}"),
            Self::Empty => f.write_str("nothing was recorded"),
        }
    }
}

/// The verdict on one channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    pub records: u64,
    /// Sequence numbers not recorded where a logged gap explains it.
    pub excused: u64,
    pub findings: Vec<Finding>,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Check one channel's segments against what the generator sent on `stream`,
/// numbers `first..=last`, and the gaps the event log recorded. Segments may
/// come in any order; they are taken in the order of their first record.
pub fn verify(
    stream: u16,
    first: u64,
    last: u64,
    mut segments: Vec<Segment>,
    gaps: &LoggedGaps,
) -> Verdict {
    let mut verdict = Verdict::default();
    for segment in &segments {
        for what in &segment.damaged {
            verdict.findings.push(Finding::Damaged {
                at: segment.name.clone(),
                what: what.clone(),
            });
        }
    }
    segments.retain(|segment| !segment.records.is_empty());
    segments.sort_by_key(|segment| segment.first());
    if segments.is_empty() {
        verdict.findings.push(Finding::Empty);
        return verdict;
    }

    // `next` is the sequence number the recording should hold next; every
    // number below it was either recorded, excused, or is in `missing`.
    let mut next = first;
    let mut missing: Vec<(u64, u64, String)> = Vec::new();
    for segment in &segments {
        let opens_after_gap = gaps.resumed_in.contains(&segment.name);
        for (position, record) in segment.records.iter().enumerate() {
            verdict.records += 1;
            if record.stream != stream {
                verdict.findings.push(Finding::WrongStream {
                    stream: record.stream,
                    at: segment.name.clone(),
                });
                continue;
            }
            let seq = record.seq;
            if seq < next {
                // A number thought missing that arrives after a later one was
                // out of order, not lost; any other was already recorded.
                let hole = missing
                    .iter()
                    .position(|(from, to, _)| (*from..=*to).contains(&seq));
                match hole {
                    Some(i) => {
                        let (from, to, at) = missing.remove(i);
                        if seq < to {
                            missing.insert(i, (seq + 1, to, at.clone()));
                        }
                        if from < seq {
                            missing.insert(i, (from, seq - 1, at));
                        }
                        verdict.findings.push(Finding::Reordered {
                            seq,
                            after: next - 1,
                            at: segment.name.clone(),
                        });
                    }
                    None => verdict.findings.push(Finding::Duplicate {
                        seq,
                        at: segment.name.clone(),
                    }),
                }
                continue;
            }
            if seq > next {
                // Only a segment that opened after a logged gap may start
                // later than the one before it ended; anything else missing
                // was lost before it was recorded.
                if position == 0 && opens_after_gap {
                    verdict.excused += seq - next;
                } else {
                    let at = if position == 0 {
                        format!("before {}", segment.name)
                    } else {
                        format!("inside {}", segment.name)
                    };
                    missing.push((next, seq - 1, at));
                }
            }
            next = seq + 1;
        }
    }
    verdict
        .findings
        .extend(
            missing
                .into_iter()
                .map(|(from, to, at)| Finding::Missing { from, to, at }),
        );
    if next <= last {
        if gaps.stopped_in_gap {
            verdict.excused += last + 1 - next;
        } else {
            verdict.findings.push(Finding::Missing {
                from: next,
                to: last,
                at: "after the last file".to_owned(),
            });
        }
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(name: &str, seqs: impl IntoIterator<Item = u64>) -> Segment {
        let bytes: Vec<u8> = seqs
            .into_iter()
            .flat_map(|seq| Record { stream: 3, seq }.encode())
            .collect();
        Segment::read(name, &bytes)
    }

    #[test]
    fn a_record_reads_back_and_damage_is_caught() {
        let record = Record {
            stream: 7,
            seq: 1_234_567_890,
        };
        let mut bytes = record.encode();
        assert_eq!(Record::decode(&bytes), Ok(record));
        bytes[30] ^= 0x01;
        assert_eq!(Record::decode(&bytes), Err("checksum mismatch"));
        bytes[0] = b'X';
        assert_eq!(Record::decode(&bytes), Err("no record marker"));
    }

    #[test]
    fn a_torn_end_is_damage() {
        let mut bytes = Record { stream: 3, seq: 0 }.encode().to_vec();
        bytes.extend_from_slice(&[1, 2, 3]);
        let read = Segment::read("GPS_2026-10-01_08.raw", &bytes);
        assert_eq!(read.records.len(), 1);
        assert_eq!(
            read.damaged,
            ["3 trailing bytes are not a whole 64-byte record"]
        );
    }

    #[test]
    fn a_whole_run_across_rotations_passes() {
        let verdict = verify(
            3,
            0,
            299,
            vec![
                segment("b.raw", 100..200),
                segment("a.raw", 0..100),
                segment("c.raw", 200..300),
            ],
            &LoggedGaps::default(),
        );
        assert!(verdict.passed(), "{:?}", verdict.findings);
        assert_eq!((verdict.records, verdict.excused), (300, 0));
    }

    #[test]
    fn a_jump_between_files_passes_only_where_a_gap_was_logged() {
        let segments = || vec![segment("a.raw", 0..100), segment("a_2.raw", 150..300)];
        let unexplained = verify(3, 0, 299, segments(), &LoggedGaps::default());
        assert_eq!(
            unexplained.findings,
            [Finding::Missing {
                from: 100,
                to: 149,
                at: "before a_2.raw".to_owned()
            }]
        );

        let log = "2026-10-01T08:00:00 WARN Raw recording on channel GPS resumed in a new \
                   file → C:\\rec\\a_2.raw channel=GPS";
        let gaps = LoggedGaps::read("GPS", log);
        assert_eq!(gaps.resumed_in, ["a_2.raw"]);
        let explained = verify(3, 0, 299, segments(), &gaps);
        assert!(explained.passed(), "{:?}", explained.findings);
        assert_eq!(explained.excused, 50);
    }

    #[test]
    fn a_hole_inside_a_file_fails_even_with_gaps_logged() {
        let mut gaps = LoggedGaps::default();
        gaps.resumed_in.push("a.raw".to_owned());
        let verdict = verify(
            3,
            0,
            9,
            vec![segment("a.raw", [0, 1, 2, 5, 6, 7, 8, 9])],
            &gaps,
        );
        assert_eq!(
            verdict.findings,
            [Finding::Missing {
                from: 3,
                to: 4,
                at: "inside a.raw".to_owned()
            }]
        );
    }

    #[test]
    fn duplicates_and_reordering_are_found() {
        // 2 arrives after 3: out of order, and so not missing. 4 arrives twice.
        let verdict = verify(
            3,
            0,
            5,
            vec![segment("a.raw", [0, 1, 3, 2, 4, 4, 5])],
            &LoggedGaps::default(),
        );
        assert_eq!(
            verdict.findings,
            [
                Finding::Reordered {
                    seq: 2,
                    after: 3,
                    at: "a.raw".to_owned()
                },
                Finding::Duplicate {
                    seq: 4,
                    at: "a.raw".to_owned()
                },
            ]
        );

        // A late arrival in the middle of a hole leaves the rest of it missing.
        let verdict = verify(
            3,
            0,
            9,
            vec![segment("a.raw", [0, 5, 2, 6, 7, 8, 9])],
            &LoggedGaps::default(),
        );
        assert_eq!(
            verdict.findings,
            [
                Finding::Reordered {
                    seq: 2,
                    after: 5,
                    at: "a.raw".to_owned()
                },
                Finding::Missing {
                    from: 1,
                    to: 1,
                    at: "inside a.raw".to_owned()
                },
                Finding::Missing {
                    from: 3,
                    to: 4,
                    at: "inside a.raw".to_owned()
                },
            ]
        );
    }

    #[test]
    fn the_ends_of_the_run_are_checked_against_what_was_sent() {
        // The tail is missing and no gap was open at the stop.
        let verdict = verify(
            3,
            0,
            99,
            vec![segment("a.raw", 0..90)],
            &LoggedGaps::default(),
        );
        assert_eq!(
            verdict.findings,
            [Finding::Missing {
                from: 90,
                to: 99,
                at: "after the last file".to_owned()
            }]
        );
        // Recording stopped during a gap: the tail is excused.
        let log = "Raw recording on channel GPS stopped during a gap: received bytes …";
        let verdict = verify(
            3,
            0,
            99,
            vec![segment("a.raw", 0..90)],
            &LoggedGaps::read("GPS", log),
        );
        assert!(verdict.passed(), "{:?}", verdict.findings);
        assert_eq!(verdict.excused, 10);
        // Another channel's lines do not count.
        assert_eq!(LoggedGaps::read("AIS", log), LoggedGaps::default());
    }

    #[test]
    fn a_record_from_another_stream_is_found() {
        let mut bytes = Record { stream: 3, seq: 0 }.encode().to_vec();
        bytes.extend_from_slice(&Record { stream: 4, seq: 1 }.encode());
        let verdict = verify(
            3,
            0,
            0,
            vec![Segment::read("a.raw", &bytes)],
            &LoggedGaps::default(),
        );
        assert_eq!(
            verdict.findings,
            [Finding::WrongStream {
                stream: 4,
                at: "a.raw".to_owned()
            }]
        );
    }

    #[test]
    fn nothing_recorded_fails() {
        let verdict = verify(3, 0, 9, Vec::new(), &LoggedGaps::default());
        assert_eq!(verdict.findings, [Finding::Empty]);
    }
}
