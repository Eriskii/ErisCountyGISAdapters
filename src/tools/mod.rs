//! The agents' tools: three command lines run in-process, like a shell without one. `web`
//! reads the public web through the polite client, `arcgis` searches for layers and inspects
//! services, and `spec` validates specs and saves results. There is no shell, no file system,
//! and no way into the registry except `spec submit`, which saves only a spec that passed.

mod arcgis;
mod spec;
pub mod text;
mod web;

pub use arcgis::Arcgis;
pub use spec::{STATEWIDE_SPEC, SpecTool};
pub use web::Web;

use crate::work::Work;
use erisharness::tools::Tool;
use std::sync::Arc;

pub fn all(work: &Arc<Work>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Web(work.clone())),
        Arc::new(Arcgis(work.clone())),
        Arc::new(SpecTool { work: work.clone(), statewide: false }),
        Arc::new(SpecTool { work: work.clone(), statewide: true }),
    ]
}

pub const COUNTY_TOOLS: [&str; 3] = ["web", "arcgis", "spec"];
pub const STATE_TOOLS: [&str; 3] = ["web", "arcgis", STATEWIDE_SPEC];

/// Shows the command line in the TUI's agent table.
fn activity(work: &Work, agent: &str, program: &str, line: &str) {
    let line = line.trim();
    let shown = if line.starts_with(program) { line.to_owned() } else { format!("{program} {line}") };
    work.with_agent(agent, |a| a.activity = text::short(&shown.replace('\n', " "), 200));
}
