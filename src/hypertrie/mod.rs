/// Sentinel ID for "unbound" (NULL) in result rows and joins.
///
/// Real dictionary IDs are consecutive starting at 0 and thus far below
/// `u32::MAX`; the maximum value is therefore usable as NULL without collision.
/// Defined centrally instead of being duplicated as a local `const` across
/// several modules.
pub const NULL_ID: u32 = u32::MAX;

pub mod dictionary;
pub mod engine;
pub mod executor;
pub mod export;
pub mod index;
pub mod planner;
pub mod query;
pub mod stats;
pub mod turtle;

pub use dictionary::{Dictionary, TermType};
pub use engine::HybridEngine;
pub use executor::{RowBlock, execute_plan, execute_wcoj, max_result_rows};
pub use export::{ParsedTerm, ParsedTriple, export_ntriples, parse_ntriples};
pub use planner::{ExecutionPlan, GraphPattern, PatternTerm, TriplePattern};
pub use query::{QueryResult, Term, TripleStore, Var};
pub use turtle::{parse_turtle, parse_turtle_str};
