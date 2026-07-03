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

mod address;
mod allocator;
mod extent;
mod extent_table;
mod free_list;
mod heap;
mod layout;
mod os_memory;
mod page_map;
mod run;
mod run_table;
mod size_class;

pub use allocator::Allocator;
