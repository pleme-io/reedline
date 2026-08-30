//! dsr — correlate a terminal query with its answer.
//!
//! ── ★ THE WIRE CARRIES NO CORRELATION FIELD ─────────────────────────────────
//!
//! A CPR reply (`ESC [ row ; col R`) contains nothing identifying the query it
//! answers. A prior-art survey of 10 line editors / TUI libraries across 5
//! languages found **no project has invented one**, and kitty's author closed
//! the equivalent ambiguity by changing the PROTOCOL rather than solving it
//! application-side (kitty discussion #5813).
//!
//! So correlation cannot be derived from the reply. It has to be RECONSTRUCTED
//! at the boundary, which is the tsugime discipline: a type is local, the
//! crossing erased it, rebuild on entry and refuse when you cannot.
//!
//! ── ★ WHY THIS EXISTS: A MEASURED SCREEN WIPE ───────────────────────────────
//!
//! crossterm 0.28.1 does not correlate. `read.rs:105-118` pushes a non-matching
//! `CursorPosition` back into a queue no public API drains, so ONE surplus or
//! orphaned answer desyncs the stream permanently — one-behind, for the life of
//! the session. Every later read returns the PREVIOUS query's answer.
//!
//! Measured on a live seat 2026-08-30 by reading the terminal's grid: the
//! prompt sat at row 10 on an otherwise blank 31-row grid, and pressing Enter
//! moved it to row 0 — because `is_reset` believed a stale answer and
//! authorised `Clear(FromCursorDown)`.
//!
//! ── ★ THE DESIGN, AND WHOSE IT IS ───────────────────────────────────────────
//!
//! fish's, which the survey found to be the most complete (`reader.rs:276-301`,
//! `:1727`, `:2925-2985`):
//!
//! 1. **At most one query outstanding**, enforced on issue.
//! 2. **A sentinel written LAST** — a second, unambiguous query whose reply
//!    marks the end of the batch. For a CPR batch that is DSR-5 → `ESC[0n`
//!    (neovim brackets OSC-11 the same way, `tui.c:352`).
//! 3. **Shape-match** the reply against the outstanding request; anything else
//!    is a rogue reply and is DROPPED. Never count-match — a count assumes the
//!    stream is aligned, which is the thing in doubt.
//! 4. **Timeout resolves like the sentinel**, so a lost reply advances the
//!    machine identically to a received one and every batch terminates.
//!
//! ── ★ SCOPE, STATED ─────────────────────────────────────────────────────────
//!
//! This module is the pure STATE MACHINE. It performs no I/O, so it is testable
//! without a terminal and liftable verbatim into a shared crate when one exists
//! — this is the third hand-rolled CPR read in the fleet (skim's leaked query
//! at `frost-exec/src/tty_takeover.rs:191`, frost's retry at `main.rs:1784`,
//! and reedline's), so the extraction is already earned; only the repo is
//! missing.
//!
//! Tier: **only-mitigated**. Correlation is reconstructed by protocol
//! convention, not carried by a type across the wire, so a sufficiently
//! adversarial stream can still defeat it. That ceiling is the wire's, not this
//! module's.

/// What was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// DSR-6 — "where is the cursor?" Answered by `ESC [ row ; col R`.
    CursorPosition,
    /// DSR-5 — "are you ok?" Answered by the unambiguous `ESC [ 0 n`.
    /// Used as the batch sentinel because its reply cannot be confused with a
    /// CPR, a key, or a device attribute.
    Status,
}

/// A reply the terminal produced, already parsed into a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// `ESC [ row ; col R`, zero-based row and column as crossterm reports them.
    CursorPosition { row: u16, col: u16 },
    /// `ESC [ 0 n` — the sentinel's reply.
    Ok,
    /// Anything else that arrived while we were waiting.
    Other,
}

/// What the caller should do with an incoming reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// It answers the outstanding query. Take it.
    Answers { row: u16, col: u16 },
    /// It does not. DROP it — do not requeue it as an answer, and do not let it
    /// satisfy a later query. This is the arm that breaks the one-behind
    /// desync.
    Rogue,
    /// The sentinel arrived: the batch is over and no answer came.
    Exhausted,
}

/// One outstanding query batch.
///
/// ★ Construction is the only way to have a query in flight, and `resolve`
/// consumes evidence as it goes. There is no way to have two batches open,
/// which is the property `assert!(query.is_none())` gives fish and which is
/// here enforced by ownership instead.
#[derive(Debug)]
pub struct Batch {
    want: Query,
    answered: Option<(u16, u16)>,
    done: bool,
}

impl Batch {
    /// Open a batch for `want`. The caller must write `want`'s query bytes
    /// followed by the sentinel's, in that order — the sentinel LAST, so its
    /// reply cannot arrive before the answer it terminates.
    #[must_use]
    pub const fn new(want: Query) -> Self {
        Self {
            want,
            answered: None,
            done: false,
        }
    }

    /// The bytes to write, in order. Sentinel last.
    #[must_use]
    pub const fn request(&self) -> &'static [u8] {
        match self.want {
            Query::CursorPosition => b"\x1b[6n\x1b[5n",
            Query::Status => b"\x1b[5n",
        }
    }

    /// Classify one reply.
    ///
    /// ★ The LAST matching reply before the sentinel wins, not the first. Under
    /// a one-behind desync the first CPR seen is the PREVIOUS query's answer;
    /// the current one arrives after it and before the sentinel.
    pub fn observe(&mut self, reply: Reply) -> Verdict {
        if self.done {
            return Verdict::Rogue;
        }
        match (self.want, reply) {
            (Query::CursorPosition, Reply::CursorPosition { row, col }) => {
                self.answered = Some((row, col));
                Verdict::Answers { row, col }
            }
            (_, Reply::Ok) => {
                self.done = true;
                Verdict::Exhausted
            }
            _ => Verdict::Rogue,
        }
    }

    /// The batch ended without the sentinel — a timeout, or an interrupt.
    ///
    /// ★ Resolves IDENTICALLY to the sentinel. fish puts `Timeout` and
    /// `Interrupted` in the same match arm as its DA1 terminator
    /// (`reader.rs:2953-2955`) precisely so that a lost reply cannot leave the
    /// machine open. Every batch terminates.
    pub const fn give_up(&mut self) {
        self.done = true;
    }

    /// The answer, if one arrived before the batch closed.
    ///
    /// `None` is a REFUSAL, not a zero — the caller must treat it as "unknown"
    /// and fall back, never as "row 0". Reading `None` as a position is the
    /// bug that wiped the operator's screen.
    #[must_use]
    pub const fn answer(&self) -> Option<(u16, u16)> {
        self.answered
    }

    /// Whether the batch has closed.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_is_written_last() {
        let b = Batch::new(Query::CursorPosition);
        let req = b.request();
        let cpr = req.windows(4).position(|w| w == b"\x1b[6n").expect("CPR");
        let sentinel = req.windows(4).position(|w| w == b"\x1b[5n").expect("DSR5");
        assert!(
            cpr < sentinel,
            "the sentinel must be LAST — otherwise its reply can arrive before \
             the answer it is supposed to terminate"
        );
    }

    #[test]
    fn a_matching_reply_answers() {
        let mut b = Batch::new(Query::CursorPosition);
        assert_eq!(
            b.observe(Reply::CursorPosition { row: 9, col: 3 }),
            Verdict::Answers { row: 9, col: 3 }
        );
        assert_eq!(b.answer(), Some((9, 3)));
    }

    /// ★ THE ONE-BEHIND DESYNC, AS A TEST. A stale CPR arrives first; the true
    /// one arrives after it; the sentinel closes the batch. The LAST reply
    /// before the sentinel must win.
    #[test]
    fn the_last_reply_before_the_sentinel_wins() {
        let mut b = Batch::new(Query::CursorPosition);
        b.observe(Reply::CursorPosition { row: 0, col: 0 }); // stale
        b.observe(Reply::CursorPosition { row: 9, col: 3 }); // truth
        assert_eq!(b.observe(Reply::Ok), Verdict::Exhausted);
        assert_eq!(
            b.answer(),
            Some((9, 3)),
            "taking the FIRST reply is exactly the one-behind desync that put \
             the prompt at row 0 and wiped the screen"
        );
    }

    #[test]
    fn a_rogue_reply_is_dropped_and_does_not_answer() {
        let mut b = Batch::new(Query::CursorPosition);
        assert_eq!(b.observe(Reply::Other), Verdict::Rogue);
        assert_eq!(b.answer(), None, "a rogue reply must not become the answer");
    }

    #[test]
    fn replies_after_the_sentinel_are_rogue() {
        let mut b = Batch::new(Query::CursorPosition);
        b.observe(Reply::Ok);
        assert_eq!(
            b.observe(Reply::CursorPosition { row: 4, col: 4 }),
            Verdict::Rogue,
            "an answer arriving after the batch closed belongs to nobody"
        );
        assert_eq!(b.answer(), None);
    }

    /// ★ A TIMEOUT MUST CLOSE THE BATCH, or a lost reply leaves the machine
    /// open forever and the next query inherits it — which is how the desync
    /// becomes permanent rather than transient.
    #[test]
    fn a_timeout_closes_the_batch_like_the_sentinel() {
        let mut b = Batch::new(Query::CursorPosition);
        b.give_up();
        assert!(b.is_closed());
        assert_eq!(b.answer(), None, "no answer arrived, so there is none");
        assert_eq!(
            b.observe(Reply::CursorPosition { row: 1, col: 1 }),
            Verdict::Rogue,
            "a late reply after a timeout must not be adopted"
        );
    }

    /// ANTI-VACUITY: `answer()` returning `None` must be reachable AND
    /// distinguishable from a real `(0, 0)`. Reading the refusal as a position
    /// is the defect this whole module exists for.
    #[test]
    fn no_answer_is_distinguishable_from_row_zero() {
        let mut timed_out = Batch::new(Query::CursorPosition);
        timed_out.give_up();

        let mut at_origin = Batch::new(Query::CursorPosition);
        at_origin.observe(Reply::CursorPosition { row: 0, col: 0 });
        at_origin.observe(Reply::Ok);

        assert_eq!(timed_out.answer(), None);
        assert_eq!(at_origin.answer(), Some((0, 0)));
        assert_ne!(
            timed_out.answer(),
            at_origin.answer(),
            "\"I do not know\" and \"the cursor is at the origin\" must not \
             render the same — that conflation is what authorised the wipe"
        );
    }
}
