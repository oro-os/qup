//! Reference parser and borrowed wire types for QUP.
//!
//! > "Every QUP message is transmitted as a single frame"
//!
//! > "The checksum is chosen so that the wrapping sum of every byte in the
//! > complete frame, in frame order and including the checksum byte, is equal
//! > to zero modulo `256`."
//!
//! This crate is `#[no_std]` and avoids allocation. Callers own transport and
//! buffer management; the crate provides borrowed wire views and parser helpers.
//!
//! # Example
//!
//! ```rust
//! use qup_core::{MessageRef, Parser, WireDirection};
//!
//! let parser = Parser::new();
//! let frame = parser
//!     .parse_frame(WireDirection::ClientToNode, &[0x3f, 0x00, 0x00, 0xc1])
//!     .unwrap();
//! let message = frame.decode_message().unwrap();
//!
//! assert_eq!(message, MessageRef::CompatibilityRequest);
//! ```
#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![deny(clippy::missing_docs_in_private_items)]
#![forbid(unsafe_code)]

pub mod io;
mod parser;
mod types;

#[doc(inline)]
pub use io::{
    AsyncByteRead, AsyncByteWrite, AsyncReadFrameError, ByteRead, ByteWrite, ReadFrameError,
};
#[doc(inline)]
pub use parser::{Parser, compute_checksum, frame_sum};
#[doc(inline)]
pub use types::{
    CapsEntry, CapsIter, CapsRef, ErrorResponse, FRAME_OVERHEAD, FrameError, FrameHeader,
    FrameView, KeyFlags, MessageRef, Opcode, OpcodeClass, OrdinaryResponseRef, PayloadCursor,
    PayloadError, RequestRef, ValueKind, ValueRef, WireDirection,
};
