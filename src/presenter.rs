//! The Presenter contract (CONTEXT.md): the display-path half of an Extension,
//! the PURE Presentment stage inside the Transcript store. ADR-0007 records the
//! lifecycle; ADR-0042 split the former combined trait into
//! [`crate::middleware::Middleware`] (execution) and Presenter (display).
//! [`crate::extensions`] runs the pipeline.
//!
//! `present` carries a **default = identity** body: an Extension that does not
//! implement Presenter (registered with no
//! Presenter role) is skipped entirely by the pipeline - observably identical
//! to the old identity default that passed every item through.
//!
//! `present` - the PURE Presentment stage inside the Transcript store. Given a
//! [`crate::view_model::TranscriptItem`] and the append's Artifacts, it
//! returns the item to display - unchanged (default = identity) or replaced
//! (e.g. a one-line Tool Result summary rewritten into a diff `Block`). No IO:
//! it runs in the view and folds, in registration order, over EVERY item the
//! store appends (ADR-0034 widened this from the two tool items) - Info and
//! Voice lines, User promotions, and settled streaming text included. Only a
//! Tool Result carries its Artifacts; every other append arrives with an empty
//! map. A Presenter therefore matches the item kinds it means to reshape and
//! passes everything else through - the identity default already does.
//!
//! Failure isolation is fail-open (ADR-0007): a crashing `present` is skipped
//! and reported as an info line in the Transcript. That isolation lives in the
//! pipeline (`catch_unwind`), not here.

use std::collections::HashMap;

use serde_json::Value;

use crate::view_model::TranscriptItem;

/// The display-path contract a Suspenders Extension implements (ADR-0042).
/// `present` defaults to identity, so a Presenter overrides it only when it
/// means to reshape items.
///
/// `opts` is the extension's registration options, carried as a
/// `serde_json::Value` - the open edge for per-extension config.
pub trait Presenter: Send + Sync {
    /// The PURE Presentment stage: folds over EVERY [`TranscriptItem`] the
    /// Transcript store appends (ADR-0034) - not just Tool Calls/Results -
    /// reading the append's Artifacts (an empty map for every non-Tool-Result
    /// append), and returns the item to display. Match the kinds you mean to
    /// reshape; pass everything else through. Default: identity. No IO.
    fn present(
        &self,
        item: TranscriptItem,
        _artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        item
    }
}
