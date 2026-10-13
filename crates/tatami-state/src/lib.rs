//! What survives a restart: the layout of every workspace on every monitor,
//! as a readable JSON file, plus named checkpoints of the same shape and
//! one layout per desk (set of monitors, `desk.rs`). The
//! conversion between this schema and the live tree lives in the reactor;
//! this crate only knows the file, the paths and the matching score that
//! puts a reappearing window back into its slot.

pub mod desk;
pub mod matcher;
pub mod paths;
pub mod schema;
pub mod store;

pub use desk::{DeskInfo, desk_id_of, desk_name, list_desks};
pub use matcher::{MatchInput, score};
pub use paths::{checkpoint_path, checkpoints_dir, desk_path, desks_dir, state_dir, state_path};
pub use schema::{
    STATE_VERSION, SavedFloating, SavedMonitor, SavedNode, SavedState, SavedWindow, SavedWorkspace,
};
pub use store::{StoreError, list_checkpoints, load, save};
