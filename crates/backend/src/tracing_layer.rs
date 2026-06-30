//! Runtime `tracing` subscriber that emits `#CLV1` hot-edge lines on span
//! enter/exit (Phase-6 hot edges, `BUILD_PLAN.md` Phase 6).
//!
//! This is the **write** side of the hot-edge seam; [`crate::clv::parse_clv_line`]
//! is the read side. A traced program annotates a call span with an `edge` field
//! (`tracing::info_span!("call", edge = "e:a->b")`), and [`HotEdgeLayer`] records
//! that id off the span: on `on_enter` it writes a `hotedge`/`enter` line and on
//! `on_close` a `hotedge`/`exit` line. Each line is the exact wire form
//! [`crate::clv::parse_clv_line`] decodes back into a
//! [`ClvEvent::HotEdge`](crate::clv::ClvEvent::HotEdge) — the round-trip contract.
//!
//! ## Throttle strategy
//! A hot loop can enter/exit the same edge thousands of times a second, which would
//! flood the collector. [`HotEdgeThrottle`] bounds this with a **per-edge,
//! time-windowed cap**: it buckets `now_millis` into fixed `W`-ms windows
//! (`window_id = now_millis / W`) and, within each window, gives every edge two
//! *independent* sub-budgets — at most [`THROTTLE_ENTER_CAP`] `enter` lines and
//! [`THROTTLE_EXIT_CAP`] `exit` lines — no matter how many transitions occur. The
//! per-window total per edge is therefore `THROTTLE_ENTER_CAP + THROTTLE_EXIT_CAP`,
//! a fixed constant, so emissions are O(1) in the transition count, not O(N). Splitting
//! the budget by state is deliberate: a re-entered span can fire `on_enter` many times
//! while `on_close` fires its lone `exit` once, so a single combined budget could spend
//! itself on enters and drop the terminal exit — leaving the edge stuck `hot`. A
//! dedicated exit sub-budget guarantees the terminal exit is never starved. The throttle
//! is pure (the caller supplies the clock), so it is deterministically testable; the one
//! impure wall-clock read lives only inside [`HotEdgeLayer`]. Rationale: per-state caps
//! per window are the cheapest structure that *provably* bounds the line rate — see the
//! `throttle_bounds_emissions_per_window` test — while still surfacing both an enter and
//! an exit sample per window. The borrow-first hot path also allocates nothing once an
//! edge is tracked, so a suppressed tight loop does no heap work.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::wire::HotEdgeState;

/// Default throttle window width in milliseconds.
///
/// Transitions are bucketed by `now_millis / `[`THROTTLE_WINDOW_MS`]; at most
/// [`THROTTLE_ENTER_CAP`] + [`THROTTLE_EXIT_CAP`] lines per edge are emitted within
/// one such window.
pub const THROTTLE_WINDOW_MS: u64 = 100;

/// Default per-window cap on `enter` lines per edge.
///
/// A small fixed constant: it bounds the per-edge `enter` line rate to at most
/// [`THROTTLE_ENTER_CAP`] lines per [`THROTTLE_WINDOW_MS`] no matter how hot the loop
/// is, which is what keeps the line-based `#CLV1` transport viable (see the module
/// docs).
pub const THROTTLE_ENTER_CAP: u32 = 2;

/// Default per-window cap on `exit` lines per edge.
///
/// Kept small but `>= 1` and budgeted *independently* of [`THROTTLE_ENTER_CAP`] so a
/// terminal `exit` is never starved by `enter` spam in the same window — the edge
/// always gets to go cold.
pub const THROTTLE_EXIT_CAP: u32 = 1;

/// Per-edge emission tally scoped to a single throttle window.
struct WindowTally {
    /// Window this tally belongs to (`now_millis / window_ms`).
    window_id: u64,
    /// `enter` lines already emitted for the edge inside `window_id`.
    enter_emitted: u32,
    /// `exit` lines already emitted for the edge inside `window_id`.
    exit_emitted: u32,
}

impl WindowTally {
    /// A fresh, empty tally for `window_id`.
    fn fresh(window_id: u64) -> Self {
        Self {
            window_id,
            enter_emitted: 0,
            exit_emitted: 0,
        }
    }
}

/// CLV hot-edge line payload, serialised to the `#CLV1` JSON body.
#[derive(serde::Serialize)]
struct HotEdgeLine<'a> {
    /// Always `"hotedge"` — the `AGENT_PROTOCOL.md` §2.3 event tag.
    event: &'static str,
    /// Originating run id.
    session: &'a str,
    /// Target edge id (`e:source->target`).
    edge: &'a str,
    /// Call-path transition, serialised `"enter"`/`"exit"`.
    state: HotEdgeState,
}

/// Pure, time-windowed, per-edge emission throttle for hot-edge lines.
///
/// Bounds the line rate so a hot loop cannot flood the collector: within each fixed
/// `window_ms` window, every edge emits at most `enter_cap` `enter` lines and
/// `exit_cap` `exit` lines — two independent sub-budgets, so a terminal `exit` is
/// never starved by `enter` spam. It is **pure** — [`note`](Self::note) takes the
/// current time as a parameter and reads no wall clock — so it is deterministically
/// testable. Throttling is **per-edge**: distinct edge ids are counted independently
/// and never coalesced together.
pub struct HotEdgeThrottle {
    /// Window width in milliseconds (clamped to at least 1 to stay panic-free).
    window_ms: u64,
    /// Maximum `enter` lines emitted per edge per window.
    enter_cap: u32,
    /// Maximum `exit` lines emitted per edge per window.
    exit_cap: u32,
    /// Live per-edge tally keyed by edge id. Not evicted: memory is bounded by the
    /// count of *distinct* edge ids (`e:<from>-><to>` over static node ids — a finite
    /// property of the traced call graph), not by transition count, so it cannot grow
    /// without bound over time.
    tallies: HashMap<String, WindowTally>,
}

impl HotEdgeThrottle {
    /// Builds a throttle with the given window width and per-window `enter`/`exit`
    /// sub-budgets.
    ///
    /// `window_ms` is clamped to at least `1` so the internal `now_millis /
    /// window_ms` bucketing can never divide by zero — panic-free on any input.
    pub fn new(window_ms: u64, enter_cap: u32, exit_cap: u32) -> Self {
        Self {
            window_ms: window_ms.max(1),
            enter_cap,
            exit_cap,
            tallies: HashMap::new(),
        }
    }

    /// Builds a throttle with the default [`THROTTLE_WINDOW_MS`] /
    /// [`THROTTLE_ENTER_CAP`] / [`THROTTLE_EXIT_CAP`] policy.
    pub fn with_defaults() -> Self {
        Self::new(THROTTLE_WINDOW_MS, THROTTLE_ENTER_CAP, THROTTLE_EXIT_CAP)
    }

    /// Records one `enter`/`exit` transition for `edge` at `now_millis` and returns
    /// whether it should be **emitted**.
    ///
    /// Returns `true` for at most `enter_cap` `enter` transitions and `exit_cap`
    /// `exit` transitions per edge per window (`window_id = now_millis / window_ms`),
    /// and `false` once that edge's sub-budget for the current window is spent; both
    /// sub-budgets reset when `now_millis` crosses into a new window. The two states
    /// are budgeted independently, so a lone terminal `exit` still emits even after
    /// the `enter` budget is exhausted. **Zero-alloc hot path:** an already-tracked
    /// edge is looked up by borrow and allocates nothing — a `String` key is only
    /// minted the first time an edge is seen.
    pub fn note(&mut self, edge: &str, state: HotEdgeState, now_millis: u64) -> bool {
        let window_id = now_millis / self.window_ms;
        // Borrow-first: only mint an owned key the first time this edge is seen, so the
        // suppressed hot-loop path (already tracked) does zero heap allocation. The
        // `get_mut(...).is_none()` guard (rather than `contains_key` + `insert`) both
        // satisfies the borrow checker — the probe borrow ends before the insert — and
        // sidesteps the `clippy::map_entry` lint, which would push us back to an
        // always-allocating `entry(edge.to_owned())`.
        if self.tallies.get_mut(edge).is_none() {
            self.tallies
                .entry(edge.to_owned())
                .or_insert_with(|| WindowTally::fresh(window_id));
        }
        let Some(tally) = self.tallies.get_mut(edge) else {
            // Unreachable: the key was just ensured present. Stay panic-free regardless.
            return false;
        };
        if tally.window_id != window_id {
            *tally = WindowTally::fresh(window_id);
        }
        let (emitted, cap) = match state {
            HotEdgeState::Enter => (&mut tally.enter_emitted, self.enter_cap),
            HotEdgeState::Exit => (&mut tally.exit_emitted, self.exit_cap),
        };
        if *emitted < cap {
            *emitted += 1;
            true
        } else {
            false
        }
    }
}

/// Formats one `#CLV1` hot-edge line (with trailing newline) for `edge`/`state`.
///
/// Produces exactly the wire form [`crate::clv::parse_clv_line`] decodes into a
/// [`ClvEvent::HotEdge`](crate::clv::ClvEvent::HotEdge): the `#CLV1 ` sentinel (with
/// its trailing space) followed by single-line JSON carrying `event`, `session`,
/// `edge`, and `state`. `state` serialises to `"enter"`/`"exit"` via
/// [`HotEdgeState`]. Serialisation cannot fail for this fixed shape; a defensive
/// fallback keeps it panic-free regardless.
pub fn format_hotedge_line(session: &str, edge: &str, state: HotEdgeState) -> String {
    let body = serde_json::to_string(&HotEdgeLine {
        event: "hotedge",
        session,
        edge,
        state,
    })
    .unwrap_or_default();
    format!("#CLV1 {body}\n")
}

/// Writes one `#CLV1` hot-edge line to `out`, swallowing any write error.
///
/// Non-blocking by contract: a failed or short write is dropped — never propagated,
/// never panics — so emitting a hot-edge line can never disturb the traced program.
/// The line content is [`format_hotedge_line`].
pub fn write_hotedge_line<W: Write>(out: &mut W, session: &str, edge: &str, state: HotEdgeState) {
    let _ = out.write_all(format_hotedge_line(session, edge, state).as_bytes());
}

/// Span-extension marker holding the `edge` id recorded off a traced span.
struct EdgeId(String);

/// Field visitor that lifts an `edge = "..."` span field into a string.
struct EdgeVisitor(Option<String>);

impl Visit for EdgeVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "edge" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "edge" && self.0.is_none() {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_owned());
        }
    }
}

/// Mutable state of a [`HotEdgeLayer`], guarded by a single mutex.
struct LayerInner<W: Write> {
    /// Per-edge emission throttle.
    throttle: HotEdgeThrottle,
    /// Injected sink for emitted lines.
    writer: W,
}

/// `tracing` layer that emits throttled `#CLV1` hot-edge lines on span enter/exit.
///
/// Records the `edge` field off each new span; on `on_enter` it emits a
/// `hotedge`/`enter` transition and on `on_close` a `hotedge`/`exit` transition for
/// that edge, each gated by a per-edge [`HotEdgeThrottle`] and written to the
/// injected `W`. Mutable state (throttle + writer) lives behind a [`Mutex`] so the
/// layer is `Send + Sync`; a poisoned lock or a write error is dropped, never
/// panicking the traced program.
pub struct HotEdgeLayer<W: Write> {
    /// Run id stamped onto every emitted line.
    session: String,
    /// Throttle + writer, behind one mutex.
    inner: Mutex<LayerInner<W>>,
}

impl<W: Write> HotEdgeLayer<W> {
    /// Builds a layer writing `session`-tagged lines to `writer` with the default
    /// [`HotEdgeThrottle::with_defaults`] policy.
    pub fn new(writer: W, session: impl Into<String>) -> Self {
        Self::with_throttle(writer, session, HotEdgeThrottle::with_defaults())
    }

    /// Builds a layer with an explicit `throttle` policy (e.g. a tuned window/cap or
    /// a test double).
    pub fn with_throttle(writer: W, session: impl Into<String>, throttle: HotEdgeThrottle) -> Self {
        Self {
            session: session.into(),
            inner: Mutex::new(LayerInner { throttle, writer }),
        }
    }

    /// Routes one transition for the span's recorded edge through the throttle and,
    /// unless suppressed, the writer. A missing edge, a poisoned lock, or a write
    /// error are all silent no-ops.
    ///
    /// The span's `extensions()` read borrow is held across the inner `Mutex` lock so
    /// the recorded edge id flows straight into the throttle and writer without a
    /// clone. This is sound: `on_new_span` only ever takes `extensions_mut` and never
    /// the inner lock, so the single lock order (span extensions → inner) has no
    /// cycle.
    fn emit<S>(&self, id: &Id, ctx: &Context<'_, S>, state: HotEdgeState)
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let Some(span) = ctx.span(id) else { return };
        let extensions = span.extensions();
        let Some(EdgeId(edge)) = extensions.get::<EdgeId>() else {
            return;
        };
        let now = now_millis();
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.throttle.note(edge, state, now) {
            write_hotedge_line(&mut inner.writer, &self.session, edge, state);
        }
    }
}

impl<S, W> Layer<S> for HotEdgeLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: Write + 'static,
{
    /// Records the span's `edge` field (if any) into its extensions so enter/exit
    /// can route it later.
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = EdgeVisitor(None);
        attrs.record(&mut visitor);
        if let Some(edge) = visitor.0 {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(EdgeId(edge));
            }
        }
    }

    /// Emits a throttled `hotedge`/`enter` line for the span's edge.
    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        self.emit(id, &ctx, HotEdgeState::Enter);
    }

    /// Emits a throttled `hotedge`/`exit` line for the span's edge.
    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        self.emit(&id, &ctx, HotEdgeState::Exit);
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch (`0` if the clock is
/// before the epoch), read only at the impure layer boundary.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clv::{parse_clv_line, ClvEvent};
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn throttle_bounds_emissions_per_window() {
        // §11.2 guard: N transitions on ONE edge in ONE window emit at most
        // enter_cap + exit_cap lines, proving emissions are O(1) in the transition
        // count under the per-state sub-budget bound.
        let bound = THROTTLE_ENTER_CAP + THROTTLE_EXIT_CAP;
        let mut throttle = HotEdgeThrottle::new(100, THROTTLE_ENTER_CAP, THROTTLE_EXIT_CAP);
        let n: u64 = 5_000;
        let mut emitted: u32 = 0;
        for i in 0..n {
            // now in 0..99 -> all map to window 0 (now / 100 == 0).
            let now = i % 100;
            let state = if i % 2 == 0 {
                HotEdgeState::Enter
            } else {
                HotEdgeState::Exit
            };
            if throttle.note("e:hot->loop", state, now) {
                emitted += 1;
            }
        }
        assert!(
            emitted <= bound,
            "emitted {emitted} lines for {n} transitions; expected <= enter_cap + exit_cap {bound}"
        );
        assert!(emitted >= 1, "at least one line should emit in the window");
    }

    #[test]
    fn throttle_tracks_edges_independently() {
        let mut throttle = HotEdgeThrottle::new(100, 1, 1);
        // Exhaust edge "a"'s enter budget in window 0.
        assert!(throttle.note("a", HotEdgeState::Enter, 0));
        assert!(
            !throttle.note("a", HotEdgeState::Enter, 0),
            "a enter budget is exhausted"
        );
        // Edge "b" keeps its own full budget despite a being spent.
        assert!(throttle.note("b", HotEdgeState::Enter, 0));
        assert!(
            !throttle.note("b", HotEdgeState::Enter, 0),
            "b enter budget is exhausted"
        );
    }

    #[test]
    fn throttle_budget_resets_next_window() {
        let mut throttle = HotEdgeThrottle::new(100, 1, 1);
        assert!(throttle.note("a", HotEdgeState::Enter, 0));
        assert!(
            !throttle.note("a", HotEdgeState::Enter, 50),
            "still window 0, enter budget spent"
        );
        assert!(
            throttle.note("a", HotEdgeState::Enter, 100),
            "window 1 resets the budget"
        );
    }

    #[test]
    fn terminal_exit_emits_even_when_enter_budget_exhausted() {
        // A re-entered span can fire on_enter many times then on_close once. The lone
        // terminal exit must still emit despite the enter budget being spent — the
        // independent exit sub-budget guarantees it (otherwise the edge stays hot).
        let mut throttle = HotEdgeThrottle::new(100, THROTTLE_ENTER_CAP, THROTTLE_EXIT_CAP);
        for _ in 0..(THROTTLE_ENTER_CAP + 2) {
            throttle.note("e:a->b", HotEdgeState::Enter, 0);
        }
        assert!(
            throttle.note("e:a->b", HotEdgeState::Exit, 0),
            "terminal exit must not be starved by enter spam"
        );
    }

    #[test]
    fn lines_round_trip_through_parse_clv_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_hotedge_line(&mut buf, "s1", "e:a->b", HotEdgeState::Enter);
        write_hotedge_line(&mut buf, "s1", "e:a->b", HotEdgeState::Exit);
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per transition");

        let cases = [(lines[0], "enter"), (lines[1], "exit")];
        for (line, want_state) in cases {
            match parse_clv_line(line) {
                Some(ClvEvent::HotEdge { edge, state, .. }) => {
                    assert_eq!(edge, "e:a->b");
                    assert_eq!(state, want_state);
                }
                other => panic!("expected HotEdge, got {other:?} for line {line}"),
            }
        }
    }

    #[test]
    fn writer_error_is_swallowed_not_propagated() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("boom"))
            }
        }
        let mut writer = FailWriter;
        // Must not panic; the error is swallowed and nothing is returned.
        write_hotedge_line(&mut writer, "s1", "e:a->b", HotEdgeState::Enter);
    }

    /// Shared buffer writer so a test can inspect what the layer emitted.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn layer_emits_enter_line_for_span_with_edge_field() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let layer = HotEdgeLayer::new(SharedBuf(sink.clone()), "s1");
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("call", edge = "e:a->b");
            let entered = span.enter();
            drop(entered);
            drop(span);
        });

        let text = String::from_utf8(sink.lock().expect("lock").clone()).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        // The layer emits both the on_enter and the on_close transition, end-to-end.
        assert_eq!(lines.len(), 2, "expected the enter and exit lines");
        match parse_clv_line(lines[0]) {
            Some(ClvEvent::HotEdge { edge, state, .. }) => {
                assert_eq!(edge, "e:a->b");
                assert_eq!(state, "enter");
            }
            other => panic!("expected HotEdge enter, got {other:?}"),
        }
        match parse_clv_line(lines[1]) {
            Some(ClvEvent::HotEdge { edge, state, .. }) => {
                assert_eq!(edge, "e:a->b");
                assert_eq!(state, "exit");
            }
            other => panic!("expected HotEdge exit, got {other:?}"),
        }
    }
}
