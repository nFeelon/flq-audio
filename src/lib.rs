pub mod aiff;
#[doc(hidden)] // exposed for the cargo-fuzz payload target, not stable API
pub mod bits;
#[doc(hidden)]
pub mod codec;
pub mod formats;
mod rc;
pub mod v1;
pub mod wav;
