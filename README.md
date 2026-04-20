<h1 align="center">
	<br>
	<br>
	<img width="320" src="assets/qup.png" alt="QUP logo">
	<br>
	<br>
	<br>
</h1>

# QUP

QUP, short for Quick Universal Protocol, is a compact binary protocol for durable state, telemetry, and request/response interactions across embedded systems. It targets small devices, mixed transports, and meshes that need a shared view of state without the implementation weight of larger, more general communication stacks.

## Goals

- Binary framing with low parser cost.
- Explicit request/response interactions.
- Durable shared state.
- The ability to subscribe to changes to values from a particular node.
- Compatibility signaling through supported opcode sets.
- Message shapes that stay simple on constrained hardware.
- A narrow scope centered on embedded key/value state with explicit node ownership, not a general-purpose message bus, RPC framework, or production queue replacement.

## Model

- Nodes own data, report telemetry, and participate in the mesh.
- Most nodes are embedded devices, but any owner of data can be a node.
- Nodes own the data they expose. Other peers may query that data or request updates, but ownership stays with the node.
- A mesh is the wider network of nodes and transports that carry QUP traffic.
- Nodes are explicitly up or down in terms of reachability.
- Up means a node is presently reachable and can exchange QUP traffic.
- Down means a node is presently unreachable.
- Reachability is explicit. It is not inferred from stale values, missing telemetry, or the age of the last successful update.
- The initial value model is intentionally small: strings, `i64`, and booleans.

QUP uses a small mandatory base vocabulary, explicit compatibility exchange, request/response semantics, and clear protocol-error boundaries.

## Data Model

- Keys are strings.
- Values are strings, signed 64-bit integers, or booleans.
- Durability is provided by the node and whatever storage backs that node.

This keeps the protocol focused on configuration, telemetry, and subscriptions without introducing a schema language into the transport layer.

## Node Sessions

For sessions with embedded nodes, repeated strings are exchanged through a session string table.

1. The peer asks the node for its string count.
2. The node returns that count.
3. For each zero-based index, the peer asks for the string at that index.
4. The peer retains that table for the life of the session.
5. Get and set operations against that node use the string index instead of resending the string.

This keeps node-directed key references fixed-size after session setup. Messages that carry string values still have variable-length payloads.

## Operations

- Discovery of nodes and their capabilities.
- Get and set operations on keys.
- Telemetry publication.
- Subscriptions to keys or key groups.
- Notifications when nodes appear, disappear, or change state.

Compatibility stays lightweight. Peers advertise which opcodes they understand, and new behavior grows by adding new requests, responses, and control messages instead of relying on version fields in the base frame.

## Workspace

- `qup-core`: shared wire types and parser scaffolding.
- `qup-embassy`: Embassy-oriented embedded integration.
- `qup`: host-side implementations and shared utilities.
- `qup-redis-bridge`: Redis-backed QUP bridge scaffold.
- `qup-tui`: ratatui-based mesh explorer scaffold.

## Documentation

- [PROTOCOL.md](PROTOCOL.md): wire framing, encodings, command vocabulary, and payload layouts.

## License

Copyright 2026 Joshua Lee Junon. Part of the [Oro Operating System Project](https://github.com/oro-os).

Licensed under either the MIT license or the Apache License, Version 2.0.