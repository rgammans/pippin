//! Pippin (sync-sets) library

// because at this stage of development there's a lot of it:
#![allow(dead_code)]

// Used for error display; not essential
#![feature(step_by)]

#![feature(box_syntax)]

// We use this. Possibly we should switch to one of the external crates.
#![feature(fs_walk)]

#![warn(missing_docs)]

extern crate crypto;
extern crate chrono;
extern crate byteorder;
extern crate hashindexed;
extern crate regex;
extern crate vec_map;
extern crate rand;

pub use detail::Repo;
pub use detail::{ElementT};
pub use detail::{PartitionState};
pub use detail::{Partition, PartitionIO, PartitionDummyIO};
pub use detail::DiscoverPartitionFiles;
pub use error::{Result};

pub mod error;
pub mod util;
mod detail;

/// Version. The low 16 bits are patch number, next 16 are the minor version
/// number, the next are the major version number. The top 16 are zero.
pub const LIB_VERSION: u64 = 0x0000_0000_0000;
