#![deny(unsafe_op_in_unsafe_fn)]

mod allocator;
mod free_list;
mod layout;
mod os_memory;
mod size_class;
mod span;
mod span_map;
mod span_table;
mod state;

pub use allocator::Allocator;
