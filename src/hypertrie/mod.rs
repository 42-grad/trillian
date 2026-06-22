/// Sentinel-ID für „ungebunden“ (NULL) in Ergebniszeilen und Joins.
///
/// Echte Dictionary-IDs sind fortlaufend ab 0 und liegen damit weit unter
/// `u32::MAX`; der Höchstwert ist daher kollisionsfrei als NULL nutzbar. Zentral
/// definiert, statt in mehreren Modulen als lokale `const` dupliziert.
pub const NULL_ID: u32 = u32::MAX;

pub mod dictionary;
pub mod engine;
pub mod executor;
pub mod export;
pub mod index;
pub mod planner;
pub mod query;
pub mod stats;

pub use dictionary::{Dictionary, TermType};
pub use engine::HybridEngine;
pub use executor::{RowBlock, execute_plan, execute_wcoj, max_result_rows};
pub use export::{ParsedTerm, ParsedTriple, export_ntriples, parse_ntriples};
pub use planner::{ExecutionPlan, GraphPattern, PatternTerm, TriplePattern};
pub use query::{QueryResult, Term, TripleStore, Var};
pub use stats::Stats;
