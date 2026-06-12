/// A detected invariant violation, pointing back into the run's history.
///
/// Predicates (§10 Phase B onward) are pure functions over
/// `&[RuntimeEvent]` returning `Result<(), Violation>`. Every predicate
/// must cite its paper section and ship with a negative test — a synthetic
/// event sequence the predicate is proven to reject.
#[derive(Debug)]
pub struct Violation {
    /// Name of the violated invariant, e.g. `"commit_monotonicity"`.
    pub invariant: &'static str,
    /// VR Revisited section the invariant comes from, e.g. `"§4.1"`.
    pub paper_section: &'static str,
    /// Virtual time of the violating event.
    pub at: u64,
    /// Index into the history of the violating event, for forensics
    /// (print the trailing N events that led here).
    pub event_index: usize,
    pub details: String,
}
