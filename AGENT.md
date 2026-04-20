# QUP Core Implementation Notes

This file is a working journal for the `crates/qup-core` reference implementation.
It exists so implementation can resume cleanly after an interruption without changing the normative spec.

## Scope

- Only implement code changes inside `crates/qup-core`.
- Keep this file as the single out-of-crate exception requested by the user for continuity.
- Do not modify any other crate or documentation file as part of the implementation.

## Task Summary

Implement a fully conforming reference implementation of the QUP parser and high-level wire types in a `#[no_std]`, no-alloc crate.

Requirements from the user request:

- parser and high-level types in `crates/qup-core`
- transport-independent traits exposed by the crate
- tests for behavior and conformance
- rustdoc documentation
- references to the normative specification, primarily as quotes

## Protocol Facts To Preserve

- Frame layout is exactly: `opcode`, `length:u16` big-endian, `payload`, `checksum`.
- Checksum rule: "the wrapping sum of every byte in the complete frame, in frame order and including the checksum byte, is equal to zero modulo `256`."
- Receiver order: read opcode, read length, read exactly payload bytes, read checksum, verify checksum, dispatch by opcode.
- Requests are `A..Z`, ordinary success responses are `a..z`, control messages are `?`, `:`, `!`, `@`.
- Any reserved or directionally invalid opcode is a protocol error.
- `bool` is only `0x00` or `0x01`.
- Variable-length fields are `u16`-prefixed and big-endian.
- `str16` must be valid UTF-8 and must not contain `0x00`.

## Proposed Crate Shape

- `types.rs`
  - opcode classification and named opcodes
  - wire enums for requests, responses, control messages
  - borrowed frame and payload view types
  - scalar and payload decoding helpers
  - spec-oriented error types
- `parser.rs`
  - checksum helpers
  - incremental frame parser over user-provided byte input
  - parser state machine that can work without allocation
  - optional helpers that decode directly from a full frame buffer
- `lib.rs`
  - crate-level docs with quoted protocol requirements
  - public re-exports for ergonomic use from embedded and host crates

## Implementation Direction

- Prefer borrowed views (`&[u8]`, `&str`) rather than owned values.
- Split implementation into small modules and small patches so interrupted work can resume safely.
- Keep parser generic over external transport by defining minimal byte-stream traits in this crate.
- Avoid embedding buffering strategy into the transport trait; let callers provide buffers.
- Separate frame parsing from semantic validation where the spec distinguishes them.
- Validate `CAPS` strings strictly according to the spec when decoding that payload.

## Likely Public API Pieces

- `Opcode`, `OpcodeClass`, and named opcode constants or enums
- `FrameHeader` and `FrameView<'a>`
- `ValueRef<'a>` for `bool`, `i64`, `str16`
- `KeyFlags`
- `ParseError` / `ProtocolError` / `PayloadError`
- transport traits such as byte reader / frame source abstractions
- parser entry points for:
  - parsing from a complete frame slice
  - incremental read into caller-owned payload storage

## Test Matrix

- valid frame parsing with zero-length and non-zero payloads
- checksum rejection
- reserved opcode rejection
- direction classification correctness
- `str16` UTF-8 validation and interior NUL rejection
- `bool` strict decoding
- `value.kind` decoding and malformed bodies
- `CAPS` validation rules, including duplicate identical pairs and malformed mismatches
- concrete examples copied from `PROTOCOL.md`
- incremental parser behavior with fragmented input

## Open Design Choices

- Transport trait naming: keep minimal and neutral so `embedded-io`, serial, sockets, and custom DMA-backed readers can adapt cleanly.
- Whether to expose both low-level byte readers and higher-level frame readers.
- How much semantic decoding should be attached directly to opcode-specific helpers versus free functions.

## Resume Point

Current status before implementation:

- existing `qup-core` crate is stub-only
- root `AGENTS.md` already exists for repo instructions and should not be repurposed
- next step is to replace the stubs in `crates/qup-core/src/types.rs`, `crates/qup-core/src/parser.rs`, and `crates/qup-core/src/lib.rs`
- implementation will proceed in smaller module files under `src/types/` and `src/parser/`

## Current Status

- `crates/qup-core` has been split into small `types/` and `parser/` modules.
- Implemented:
  - opcode classification and direction validation
  - frame header/view types
  - payload cursor and scalar decoders
  - `value` decoding
  - `CAPS` validation and iteration
  - typed message decoding
  - checksum helpers
  - frame parsing from slices and transport readers
  - transport traits `ByteRead` and `ByteWrite`
- Tests currently pass with `cargo test -p qup-core`.

If work resumes later, start by checking whether the remaining need is API expansion or tighter documentation, not missing basic parser functionality.