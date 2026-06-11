# fleet-conductor

Fleet orchestration conductor for distributed agent coordination.

**Status:** Early stage — scaffolded, building, tests passing.

## What it does

Coordinates multiple fleet agents running concurrently — task assignment,
health checks, and graceful shutdown. Sits above individual agents and
below whatever is issuing commands to the fleet.

## Building

```sh
cargo build
cargo test
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
