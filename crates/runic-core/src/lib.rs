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

pub(crate) mod address;
pub(crate) mod allocator;
pub(crate) mod extent;
pub(crate) mod extent_table;
pub(crate) mod free_list;
pub(crate) mod heap;
pub(crate) mod layout;
pub(crate) mod mapping_cache;
pub(crate) mod os_memory;
pub(crate) mod page_map;
pub(crate) mod run;
pub(crate) mod run_table;
pub(crate) mod size_class;
pub(crate) mod slot_store;

pub use allocator::Allocator;
