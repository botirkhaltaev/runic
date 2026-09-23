//! Location of the interceptor under test.
//!
//! Cargo builds `runic-cabi` as a cdylib artifact dependency and reports the
//! resulting path here, so tests never have to guess a target directory.

/// Absolute path of the `librunic.so` built for this profile.
pub const LIBRARY: &str = env!("CARGO_CDYLIB_FILE_RUNIC_CABI_runic");
