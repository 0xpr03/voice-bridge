# Teamspeak3 <-> Discord Voice Bridge

Requires your own discord bot token.

This software is in an MVP status, use at your own risk, like always.

## Building
get [rust](https://rust-lang.org) compiler with cargo

`cargo build --release`
.exe/elf is inside target/release/

## Starting
Setup your credentials inside .credentials.toml by copying credentials.example.toml


Then join a voice channel, type ~join in a text channel the bot can access.

## License

voice_bridge is primarily distributed under the terms of the AGPL license (Version 3.0). Libraries specified by the cargo.toml and code annotated otherwise is copyright by their respective authors.

See LICENSE-AGPL details.