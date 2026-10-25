Build a production-quality Rust service in this empty crate, called ledger: a small accounts ledger (create account, deposit, withdraw, transfer, get balance; balances never negative; transfers atomic).

Requirements:
- An HTTP JSON API with axum and a gRPC API with tonic, both served by one binary on separate ports, sharing one store behind a trait with an in-memory implementation.
- The gRPC service defined in proto/ledger.proto and generated with tonic-build in build.rs.
- Errors with thiserror, mapped to proper HTTP status codes and gRPC status codes.
- Structured logging with tracing, configuration from environment variables with sensible defaults, graceful shutdown on Ctrl-C.
- A lib plus a thin main, so tests can start the server in-process.
- Unit tests for the domain logic, and integration tests in tests/ that start both servers on ephemeral ports and exercise every operation over HTTP and over gRPC, including the error cases and a concurrent-transfers test.
- cargo clippy clean, cargo fmt applied.

Run the build, clippy and the tests yourself and keep going until everything passes. Finish with a short summary of the design and what you verified.
