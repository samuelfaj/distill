use super::{FenceDecision, TeardownFence};
use crate::app::reader_thread::ReaderJoin;

/// Rows are (reader, writer_timed_out, flags_pushed, expected).
#[test]
fn decide_truth_table() {
    use FenceDecision::{Run, SkipNoFlags, SkipReaderAlive, SkipWriterWedged};
    use ReaderJoin::{Absent, Joined, TimedOut};

    let table = [
        (Joined, false, true, Run),
        (Absent, false, true, Run),
        (Joined, false, false, SkipNoFlags),
        (Absent, false, false, SkipNoFlags),
        (TimedOut, false, true, SkipReaderAlive),
        (TimedOut, false, false, SkipReaderAlive),
        (Joined, true, true, SkipWriterWedged),
        (Joined, true, false, SkipWriterWedged),
        (Absent, true, true, SkipWriterWedged),
        (Absent, true, false, SkipWriterWedged),
        (TimedOut, true, true, SkipWriterWedged),
        (TimedOut, true, false, SkipWriterWedged),
    ];
    for (reader, writer_timed_out, flags_pushed, expected) in table {
        let fence = TeardownFence {
            reader,
            writer_timed_out,
            flags_pushed,
        };
        assert_eq!(
            expected,
            fence.decide(),
            "reader={reader:?} writer_timed_out={writer_timed_out} flags_pushed={flags_pushed}"
        );
    }
}
