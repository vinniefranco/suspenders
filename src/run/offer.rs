//! Offer - the Tools one Pass puts before the model (CONTEXT.md: Offer).
//!
//! The narrowed specs move in at the request-shaping moment, after the
//! Governors' NarrowTools Intervention. One value with two readers - the
//! request carries [`Offer::specs`] and the batch's dispatch check asks
//! [`Offer::offers`] - so the enforced set and the wire set cannot drift
//! (ADR-0035).

use crate::tool::ToolSpec;

pub(super) struct Offer {
    specs: Vec<ToolSpec>,
}

impl Offer {
    /// Shapes the Pass's Offer: the narrowed specs move in.
    pub(super) fn new(specs: Vec<ToolSpec>) -> Self {
        Offer { specs }
    }

    /// Whether this Pass offered the named Tool - the batch refuses any Tool
    /// Call this answers `false` for (ADR-0035).
    pub(super) fn offers(&self, name: &str) -> bool {
        self.specs.iter().any(|spec| spec.name == name)
    }

    /// What the wire carries: the request reads the specs back out of the
    /// Offer, so it holds exactly what the batch enforces.
    pub(super) fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }
}

/// The pre-first-request state: offers nothing. The batch can never run
/// before the first request is shaped.
impl Default for Offer {
    fn default() -> Self {
        Offer { specs: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("The {name} tool."),
            input_schema: json!({ "type": "object" }),
        }
    }

    #[test]
    fn an_offer_names_what_it_was_shaped_from_and_refuses_what_it_was_not() {
        // ADR-0035: the batch refuses any Tool Call the Offer does not name.
        let offer = Offer::new(vec![spec("read_file"), spec("run_command")]);
        assert!(offer.offers("read_file"));
        assert!(offer.offers("run_command"));
        assert!(!offer.offers("write_file"));
    }

    #[test]
    fn the_default_offer_offers_nothing() {
        // The pre-first-request state (CONTEXT.md: Offer): before the first
        // request is shaped, the Offer offers nothing.
        let offer = Offer::default();
        assert!(!offer.offers("read_file"));
        assert!(offer.specs().is_empty());
    }

    #[test]
    fn specs_returns_exactly_what_moved_in() {
        // The request carries exactly what the Offer holds.
        let offer = Offer::new(vec![spec("grep"), spec("list_files")]);
        let names: Vec<String> = offer.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["grep", "list_files"]);
    }
}
