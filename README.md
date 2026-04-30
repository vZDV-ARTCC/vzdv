# vzdv

![lang](https://img.shields.io/badge/lang-rust-orange)
![licensing](https://img.shields.io/badge/license-MIT_or_Apache_2.0-blue)
![status](https://img.shields.io/badge/project_status-production-green)
![CI](https://github.com/vzdv-artcc/vzdv/actions/workflows/pr-ci.yml/badge.svg)

New vZDV website.

This site is not affiliated with the Federal Aviation Administration, actual Denver ARTCC, or any real-world governing aviation body.
All content herein is solely for use on the [VATSIM network](https://vatsim.net/).

## Project goals

- Provide a website solution for the vZDV VATUSA ARTCC.
- Be both fast and lightweight.
- Be easy to use.
- Follow good software development practices.
- Be easy to develop, deploy, and run.

## Building

### Requirements

- Git
- A recent version of [Rust](https://www.rust-lang.org/tools/install)

### Steps

```sh
git clone https://github.com/vzdv-artcc/vzdv
cd vzdv
cargo build
```

This app follows all [Clippy](https://doc.rust-lang.org/clippy/) lints on _Nightly Rust_. You can use either both a stable and nightly toolchain, or just a nightly (probably; I use the dual setup). If using both, execute clippy with `cargo +nightly clippy`. You do not need this for _running_ the app, just developing on it.

## Running

This project contains multiple binaries. From the project root, you can run `cargo run --bin vzdv-site` to start the site. If you build and export the binaries (`cargo b --release`, ...), just execute the correct binary.

You'll need to create a configuration file. An empty layout example is supplied [here](./vzdv.sample.toml). You can put this file anywhere on the system and point to it with the `--config <path>` flag; if the file is in the same directory as the binary and named "vzdv.toml", you do not need to supply the flag.

Additional CLI parameters can be found by running each binary with the `--help` flag.

## Deploying

This app makes few assertions about how it should be ran. You can run it directly, run triggered by a systemd unit file, run in a Docker container, etc. You _will_ need to have this app behind some sort of reverse proxy that provides HTTPS, like [Caddy](https://caddyserver.com/), as handling TLS termination is not something that this app does or will handle.

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

Loading indicator from [SamHerbert/SVG-Loaders](https://github.com/SamHerbert/SVG-Loaders). Geo boundary data from [vatspy-data-project](https://github.com/vatsimnetwork/vatspy-data-project). HTML table sorting JS from [kryogenix.org](https://www.kryogenix.org/code/browser/sorttable). See 'Cargo.toml' files for a list of used Rust libraries.

## Contributing

This repo is happily FOSS, but any contributions will need to fall in line with my vision of how the site looks and works.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
