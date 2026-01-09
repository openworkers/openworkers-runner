# OpenWorkers Runner - Technical Documentation

This directory contains technical documentation about the runner's architecture, design decisions, and important implementation details.

## Documents

### Architecture
- **[Worker Pool Architecture](./worker-pool.md)** - Current architecture: sequential worker pool with disposable isolates
- **[Isolate Pooling Architecture](./isolate-pooling.md)** - Proposed architecture: persistent workers with isolate reuse (like Cloudflare)
- **[Runtime Safety Guidelines](./runtime-safety.md)** - Important rules for working with multiple tokio runtimes

### Bug Reports & Fixes
- **[HTTP Client Thread-Local Fix](./http-client-thread-local.md)** - Why we use thread-local HTTP clients (production bug fix)

### Performance
- **[Configuration](./configuration.md)** - Environment variables and tuning parameters

### Known Issues
- **[Isolate Pooling](./isolate-pooling.md)** - Current architecture creates/destroys isolates per request (inefficient). Proposed solution: persistent workers.

## Quick Reference

### Key Design Decisions

1. **One V8 isolate per worker** - Ensures LIFO drop order safety
2. **Sequential execution per thread** - One worker at a time per worker thread
3. **Thread-local HTTP clients** - Each worker thread gets its own reqwest::Client
4. **Multi-threaded worker runtimes** - Each worker thread has a multi-threaded tokio runtime (2 worker threads)
5. **Fresh LocalSet per worker** - Prevents spawn_local contamination between workers

### Critical Rules

⚠️ **Never** use global `reqwest::Client` - always use thread-local
⚠️ **Never** share LocalSet across workers - create fresh per worker
⚠️ **Always** use `once_cell::unsync::Lazy` for thread-local initialization

## Contributing

When adding new runtime-bound resources (HTTP clients, database pools, etc.), refer to [Runtime Safety Guidelines](./runtime-safety.md) for best practices.
