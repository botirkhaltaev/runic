#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(
    not(test),
    warn(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::indexing_slicing,
        clippy::undocumented_unsafe_blocks
    )
)]

pub(crate) mod allocation;
pub(crate) mod allocator;
pub mod config;
pub(crate) mod extent;
pub(crate) mod heap;
pub(crate) mod layout;
pub(crate) mod memory;
pub(crate) mod run;
pub(crate) mod size_class;
pub(crate) mod slot_store;

pub use allocator::Allocator;
pub use config::{
    AllocatorConfig, Budget, ExtentConfig, ExtentPolicy, ExtentReuse, RunConfig, RunPolicy,
};
