//! Compose errors at the heap / slot edge. Domain leaves stay on `Run` / `Extent` / `PageMap`.

use super::{extent::ExtentError, run::RunError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    InvalidHeap,
    InvalidRunPointer,
    InvalidExtentPointer,
    DoubleFree,
    InvalidMetadata,
    MissingExtent,
}

impl From<RunError> for HeapError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::InvalidPointer => Self::InvalidRunPointer,
            RunError::DoubleFree => Self::DoubleFree,
        }
    }
}

impl From<ExtentError> for HeapError {
    fn from(error: ExtentError) -> Self {
        match error {
            ExtentError::InvalidPointer => Self::InvalidExtentPointer,
            ExtentError::DoubleFree => Self::DoubleFree,
        }
    }
}
