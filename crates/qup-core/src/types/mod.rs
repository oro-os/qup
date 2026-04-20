//! Borrowed wire-level and decoded payload types for QUP.
//!
//! > "All integers are transmitted in big-endian format."
//!
//! > "Variable-length data is prefix-encoded."
//!
//! > "`bool` values are encoded as `0x00` or `0x01`; any other value is a
//! > protocol error."

mod caps;
mod cursor;
mod error;
mod frame;
mod message;
mod opcode;
mod scalar;
mod value;

pub use caps::{CapsEntry, CapsIter, CapsRef};
pub use cursor::PayloadCursor;
pub use error::{FrameError, PayloadError};
pub use frame::{FRAME_OVERHEAD, FrameHeader, FrameView};
pub use message::{ErrorResponse, MessageRef, OrdinaryResponseRef, RequestRef};
pub use opcode::{Opcode, OpcodeClass, WireDirection};
pub use scalar::{KeyFlags, decode_bool};
pub use value::{ValueKind, ValueRef};
