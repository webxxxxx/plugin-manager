//! Test-only driver for sans-IO stream handlers.
//!
//! The handler is the seam; this drives it exactly as the server loop does so
//! each face test exercises the same event sequence.

use crate::server::http::{
    CloseReason, JobBudget, OutBuffer, StreamAction, StreamEvent, StreamHandler,
};

/// Whether a `SendFile` action ends the drive or the handler continues with
/// another writable event.
pub(crate) enum SendFilePolicy {
    Continue,
    Finish,
}

/// Drive one sans-IO stream handler the way the server does: one bounded output
/// buffer, submitted jobs run inline, file sends performed, then a fresh
/// Writable event. `blocking` lets a handler that never submits a job keep the
/// same loop (and panic if it does).
pub(crate) fn drive_stream<S: StreamHandler>(
    handler: &mut S,
    limit: usize,
    send: SendFilePolicy,
    blocking: bool,
) -> (Vec<u8>, CloseReason) {
    use std::io::{Read, Seek, SeekFrom};

    let mut all = Vec::new();
    let mut out = Vec::new();
    let mut step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
    for _ in 0..100_000 {
        assert!(
            out.len() <= limit,
            "a writable event must respect the output buffer limit"
        );
        all.extend_from_slice(&out);
        out.clear();
        match step.action {
            StreamAction::Close(reason) => return (all, reason),
            StreamAction::SendFile(mut send_file) => {
                send_file
                    .file
                    .seek(SeekFrom::Start(send_file.offset))
                    .expect("seek file send");
                let mut reader = (&mut send_file.file).take(send_file.length);
                reader.read_to_end(&mut all).expect("read file send");
                if matches!(send, SendFilePolicy::Finish) {
                    return (all, CloseReason::Completed);
                }
            }
            StreamAction::SubmitBlocking(job) => {
                assert!(blocking, "this handler submits no blocking job");
                let outcome = job(JobBudget::for_test());
                step = handler.on_event(
                    StreamEvent::BlockingDone(outcome),
                    &mut OutBuffer::new(&mut out, limit),
                );
                continue;
            }
            StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {}
        }
        step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
    }
    panic!("the handler never finished");
}
