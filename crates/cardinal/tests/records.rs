//! Records (`docs/architecture.md` section 4.7): torn tails cost at most the torn line, and
//! rotation keeps the newest records in order without losing or splitting lines, even with
//! an appender paused between opening the file and writing.

mod support;

use std::io::Write;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use cardinal::contract::{Record, RecordKind};
use cardinal::record::{Recorder, read_all};
use cardinal::state::{self, LockMode, StateDir, try_lock};
use cardinal::testpoint::RECORD_OPENED;
use proptest::prelude::*;
use support::helper::{self, ChildGuard, Mode};
use support::testpoints::TestPoints;

#[test]
fn helper() {
    helper::run_if_requested();
}

fn state() -> (tempfile::TempDir, StateDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::open(dir.path()).unwrap();
    (dir, state)
}

fn numbered(recorder: &Recorder, seq: u64, writer: u64) -> Record {
    let mut record = recorder.record(RecordKind::JobFinished);
    record.job_id = Some(format!("{writer:04x}{seq:012x}"));
    record.data = serde_json::from_value(
        serde_json::json!({ "seq": seq, "writer": writer, "pad": "x".repeat(40) }),
    )
    .unwrap();
    record
}

fn seq(record: &Record) -> u64 {
    record.data["seq"].as_u64().unwrap()
}

#[test]
fn records_round_trip_in_order() {
    let (_dir, state) = state();
    let recorder = Recorder::new(&state, 1 << 20, 4);
    for n in 0..20 {
        recorder.append(&numbered(&recorder, n, 0)).unwrap();
    }
    let records = read_all(&state).unwrap();
    assert_eq!(
        records.iter().map(seq).collect::<Vec<_>>(),
        (0..20).collect::<Vec<_>>()
    );
    assert!(records.iter().all(|r| r.host_id == state.host_id()));
}

#[test]
fn torn_tail_costs_only_the_torn_line() {
    let (_dir, state) = state();
    let recorder = Recorder::new(&state, 1 << 20, 4);
    recorder.append(&numbered(&recorder, 0, 0)).unwrap();
    recorder.append(&numbered(&recorder, 1, 0)).unwrap();
    // A crash in the middle of the third append.
    let torn = serde_json::to_string(&numbered(&recorder, 2, 0)).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(state.records_file())
        .unwrap();
    file.write_all(&torn.as_bytes()[..torn.len() / 2]).unwrap();
    drop(file);

    assert_eq!(
        read_all(&state)
            .unwrap()
            .iter()
            .map(seq)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    recorder.append(&numbered(&recorder, 3, 0)).unwrap();
    assert_eq!(
        read_all(&state)
            .unwrap()
            .iter()
            .map(seq)
            .collect::<Vec<_>>(),
        [0, 1, 3],
        "the append after the torn line survives"
    );
}

#[test]
fn foreign_and_garbage_lines_are_skipped() {
    let (_dir, state) = state();
    let recorder = Recorder::new(&state, 1 << 20, 4);
    recorder.append(&numbered(&recorder, 0, 0)).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(state.records_file())
        .unwrap();
    file.write_all(b"not json\n\n{\"schema\":\"cardinal.record.v2\",\"ts_ms\":1}\n\xff\xfe\n")
        .unwrap();
    drop(file);
    recorder.append(&numbered(&recorder, 1, 0)).unwrap();
    assert_eq!(
        read_all(&state)
            .unwrap()
            .iter()
            .map(seq)
            .collect::<Vec<_>>(),
        [0, 1]
    );
}

#[test]
fn rotation_keeps_the_newest_files_in_order() {
    let (_dir, state) = state();
    let line_len = serde_json::to_vec(&numbered(&Recorder::new(&state, 1, 1), 0, 0))
        .unwrap()
        .len() as u64
        + 1;
    // Rotate after every fourth record; keep the current file and two rotated ones.
    let recorder = Recorder::new(&state, line_len * 4 - 1, 3);
    for n in 0..40 {
        recorder.append(&numbered(&recorder, n, 0)).unwrap();
    }
    assert!(state.rotated_records(1).exists());
    assert!(state.rotated_records(2).exists());
    assert!(
        !state.rotated_records(3).exists(),
        "keep = 3 counts the current file"
    );
    let seqs: Vec<u64> = read_all(&state).unwrap().iter().map(seq).collect();
    let first = seqs[0];
    assert_eq!(
        seqs,
        (first..40).collect::<Vec<_>>(),
        "contiguous, newest kept"
    );
    for path in [state.rotated_records(1), state.rotated_records(2)] {
        let len = std::fs::metadata(path).unwrap().len();
        assert!(
            len >= line_len * 4 - 1,
            "files rotate only once over the limit"
        );
    }
}

#[test]
fn keep_one_drops_the_full_file() {
    let (_dir, state) = state();
    let recorder = Recorder::new(&state, 10, 1);
    recorder.append(&numbered(&recorder, 0, 0)).unwrap();
    assert!(!state.records_file().exists());
    assert!(!state.rotated_records(1).exists());
}

#[test]
fn rotation_waits_for_open_appenders() {
    let (_dir, state) = state();
    let recorder = Recorder::new(&state, 10, 4);
    // An appender between opening the file and writing.
    let held = try_lock(&state.records_lock(), LockMode::Shared)
        .unwrap()
        .unwrap();
    recorder.append(&numbered(&recorder, 0, 0)).unwrap();
    recorder.append(&numbered(&recorder, 1, 0)).unwrap();
    assert!(
        !state.rotated_records(1).exists(),
        "rotation is left to a later append"
    );
    drop(held);
    recorder.append(&numbered(&recorder, 2, 0)).unwrap();
    assert!(state.rotated_records(1).exists());
    assert!(!state.records_file().exists());
    assert_eq!(
        read_all(&state)
            .unwrap()
            .iter()
            .map(seq)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

#[test]
fn append_gives_up_while_a_rotation_holds_the_lock() {
    let (_dir, state) = state();
    let recorder = Recorder::new(&state, 1 << 20, 4);
    let held = try_lock(&state.records_lock(), LockMode::Exclusive)
        .unwrap()
        .unwrap();
    let start = Instant::now();
    assert!(recorder.append(&numbered(&recorder, 0, 0)).is_err());
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the append waits a bounded time"
    );
    assert!(!state.records_file().exists(), "nothing is written");
    drop(held);
    recorder.append(&numbered(&recorder, 1, 0)).unwrap();
    assert_eq!(
        read_all(&state)
            .unwrap()
            .iter()
            .map(seq)
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn a_paused_appender_loses_nothing_across_two_rotations() {
    let (dir, state) = state();
    // Every append exceeds the limit, and with keep = 2 a second rotation replaces
    // records.1.jsonl: an appender writing through a descriptor opened before both would
    // write into a removed file.
    let (max_bytes, keep) = (1, 2);
    let recorder = Recorder::new(&state, max_bytes, keep);
    recorder.append(&numbered(&recorder, 0, 0)).unwrap();
    assert!(state.rotated_records(1).exists());

    let points = TestPoints::new();
    let mut command = helper::command(&Mode::Append {
        state: dir.path().to_path_buf(),
        seq: 1,
        max_bytes,
        keep,
    });
    points.apply(&mut command, &[RECORD_OPENED], None);
    let mut paused = ChildGuard::new(command.spawn().unwrap());
    points.wait_reached(RECORD_OPENED, paused.id(), Duration::from_secs(30));

    // Two appends that would each rotate.
    recorder.append(&numbered(&recorder, 2, 0)).unwrap();
    recorder.append(&numbered(&recorder, 3, 0)).unwrap();
    assert_eq!(
        state::read_jsonl::<Record>(&state.records_file())
            .unwrap()
            .iter()
            .map(seq)
            .collect::<Vec<_>>(),
        [2, 3],
        "no rotation while the paused appender holds the file"
    );

    points.release(RECORD_OPENED);
    let status = paused.child().wait().unwrap();
    assert!(status.success(), "the paused append failed: {status}");
    let seqs: Vec<u64> = read_all(&state).unwrap().iter().map(seq).collect();
    assert_eq!(seqs, [2, 3, 1], "the paused record survives");
}

#[test]
fn concurrent_appenders_with_rotation_never_split_lines() {
    let (_dir, state) = state();
    let writers: u64 = 8;
    let per_writer: u64 = 200;
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..writers)
        .map(|writer| {
            let state = state.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let recorder = Recorder::new(&state, 16 * 1024, 64);
                barrier.wait();
                for n in 0..per_writer {
                    recorder.append(&numbered(&recorder, n, writer)).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let records = read_all(&state).unwrap();
    assert_eq!(
        u64::try_from(records.len()).unwrap(),
        writers * per_writer,
        "nothing lost or torn"
    );
    for writer in 0..writers {
        let seqs: Vec<u64> = records
            .iter()
            .filter(|r| r.data["writer"] == writer)
            .map(seq)
            .collect();
        assert_eq!(seqs, (0..per_writer).collect::<Vec<_>>(), "writer {writer}");
    }
    let raw: usize = std::fs::read_dir(state.records_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .map(|p| {
            std::fs::read(p)
                .unwrap()
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .count()
        })
        .sum();
    assert_eq!(raw, records.len(), "every line parses");
}

proptest! {
    /// A crash may cut the file at any byte. Reading returns exactly the records whose JSON is
    /// complete before the cut, and a later append is never lost.
    #[test]
    fn any_cut_loses_only_the_cut_record(count in 1u64..12, cut_seed in any::<u64>()) {
        let (_dir, state) = state();
        let recorder = Recorder::new(&state, 1 << 20, 4);
        let mut ends: Vec<(u64, u64)> = Vec::new();
        let mut len = 0u64;
        for n in 0..count {
            let record = numbered(&recorder, n, 0);
            len += serde_json::to_vec(&record).unwrap().len() as u64;
            ends.push((n, len));
            len += 1;
            recorder.append(&record).unwrap();
        }
        let cut = cut_seed % (len + 1);
        let file = std::fs::OpenOptions::new().write(true).open(state.records_file()).unwrap();
        file.set_len(cut).unwrap();
        drop(file);

        let complete: Vec<u64> = ends.iter().filter(|(_, end)| *end <= cut).map(|(n, _)| *n).collect();
        let read: Vec<u64> = read_all(&state).unwrap().iter().map(seq).collect();
        prop_assert_eq!(&read, &complete);

        recorder.append(&numbered(&recorder, 100, 0)).unwrap();
        let mut expected = complete;
        expected.push(100);
        let read: Vec<u64> = read_all(&state).unwrap().iter().map(seq).collect();
        prop_assert_eq!(read, expected);
    }
}
