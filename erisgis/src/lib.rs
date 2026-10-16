//! Declarative adapters for the public GIS layers US counties publish, one dataset at a time
//! (tax parcels, zoning districts): specs that point at a county's layer and map its attributes
//! to the dataset's normalized fields, the registry that holds them, the runtime that reads
//! features through them, the validator that proves a spec works, and the lookup that reads a
//! map view's features. ErisGISScraper's agents write the specs; ErisMaps reads through them.

pub mod counties;
pub mod dataset;
pub mod geom;
pub mod http;
pub mod lookup;
pub mod parcels;
pub mod registry;
pub mod runtime;
pub mod spec;
#[cfg(feature = "testing")]
pub mod testing;
pub mod validate;
pub mod zoning;

pub use dataset::Dataset;

/// Sent with every request to a county's servers, so their administrators know who is asking.
pub const USER_AGENT: &str = concat!(
    "ErisGIS/",
    env!("CARGO_PKG_VERSION"),
    " (public GIS adapter reader and validator; +https://github.com/Eriskii)"
);
