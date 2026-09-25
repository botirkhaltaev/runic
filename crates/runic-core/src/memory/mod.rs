mod address;
mod os;
mod page_map;

pub(crate) use address::AddressRange;
pub(crate) use os::{Mapping, Memory, PAGE_SIZE};
pub(crate) use page_map::{PageMap, PageOwner};

/// The platform [`Memory`] impl. Porting swaps this alias (roadmap 0.14);
/// callers never name an OS.
pub(crate) type Os = os::Linux;
