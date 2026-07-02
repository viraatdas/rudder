# Native TUI Profiling

Rudder's native dashboard keeps diagnostics quiet by default. An idle dashboard
should not write perf logs.

## Live HUD

Use the in-terminal HUD when chasing scroll or frame latency:

```sh
RUDDER_PERF_HUD=1 rudder
```

The HUD shows recent frame, draw, and PTY-drain timing without writing to disk.

## Perf NDJSON

Enable file capture only for a profiling run:

```sh
RUDDER_NATIVE_PERF=1 rudder
```

Logs are written under `~/.rudder` as per-process files named
`native-perf-<pid>.ndjson`. They contain periodic percentile summaries plus raw
events only for slow outliers, not every frame.

## External Profilers

For CPU attribution, prefer sampling tools over adding always-on log firehoses:

```sh
cargo build --release --manifest-path native/Cargo.toml
samply record ./target/release/rudder-native
```

`cargo flamegraph` is also useful for one-shot profiles. Use `rudder gc --dry-run`
to inspect diagnostics and old managed binaries when checking disk usage.
