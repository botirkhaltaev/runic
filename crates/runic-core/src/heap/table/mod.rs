mod remote;
mod slot;
mod thread;

pub(crate) use slot::{HeapError, HeapHandle, HeapTable};
pub(crate) use thread::THREAD_HEAP;
