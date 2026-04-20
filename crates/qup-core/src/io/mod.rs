//! Transport-facing sync and async helpers.
//!
//! The wire format itself is transport-independent; these modules provide the
//! minimal traits and frame readers needed to adapt external transports.

/// Synchronous transport traits and frame readers.
pub mod sync;

/// Asynchronous transport traits and frame readers.
pub mod asynch;

#[doc(inline)]
pub use asynch::{
    AsyncByteRead, AsyncByteWrite, AsyncReadFrameError, read_frame as read_frame_async,
};
#[doc(inline)]
pub use sync::{ByteRead, ByteWrite, ReadFrameError, read_frame};
