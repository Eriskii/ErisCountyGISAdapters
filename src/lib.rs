//! Parcel adapters for every US county: declarative specs that point at a county's public
//! parcel service, the runtime that reads parcels through them, the validator that proves a
//! spec works, and the agents and scheduler that write them.

pub mod assist;
pub mod cli;
pub mod counties;
pub mod geom;
pub mod http;
pub mod journal;
pub mod lookup;
pub mod models;
pub mod prompts;
pub mod registry;
pub mod runtime;
pub mod scheduler;
pub mod spec;
pub mod tools;
pub mod tui;
pub mod validate;
pub mod view;
pub mod work;

/// Sent with every request to a county's servers, so their administrators know who is asking.
pub const USER_AGENT: &str = concat!(
    "ErisCountyGISAdapters/",
    env!("CARGO_PKG_VERSION"),
    " (parcel adapter validator; +https://github.com/Eriskii)"
);
