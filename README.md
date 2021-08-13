# Teamspeak3 <-> Discord Voice Bridge

Requires your own discord bot token.

This software is in an MVP status, use at your own risk, like always.

## Building
get [rust](https://rust-lang.org) compiler with cargo

Then run `cargo build --release`
.exe/elf is inside target/release/
You can also run `cargo run --release` instead to directly build & execute the resulting binary.

### Build optimization

The default release build is heavily optimized, using native target-cpu instructions and LTO. You can disable LTO in the Cargo.toml under `[profile.release]`, which can reduce the build time by a lot. And you can disable the target-cpu flags in `.cargo/config.toml`.

## Starting
Setup your credentials inside .credentials.toml by copying credentials.example.toml

Then join a voice channel in discord, type ~join in a text channel the bot can access. The teamspeak side should already be connected based on your config.

## Debugging

To enable backtrace you can set the `RUST_BACKTRACE` environment variable like so:
On linux run with `RUST_BACKTRACE=1` (so `RUST_BACKTRACE=1 cargo run --release`)
On windows execute `$Env:RUST_BACKTRACE='1'` in your powershell (I recommend windows terminal). Then run the binary from there, see above.

Logging can be controlled via `RUST_LOG=<value>` environment variable with `<value>` being one of error, warn, info, debug, trace. See above for setting it.

## License

voice_bridge is primarily distributed under the terms of the AGPL license (Version 3.0). Libraries specified by the cargo.toml and code annotated otherwise is copyright by their respective authors.

See LICENSE-AGPL details.