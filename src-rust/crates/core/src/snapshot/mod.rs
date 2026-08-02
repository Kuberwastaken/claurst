pub mod registry;
pub mod shadow;
pub mod types;

pub use registry::{get_or_create, remove};
pub use shadow::ShadowSnapshot;
pub use types::{FileDiff, FileStatus, Patch};
