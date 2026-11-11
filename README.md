# Lightair

A native live-production studio for streamers and creators, built on **libobs** (the engine inside OBS Studio) with an interface written in **Rust** on **GPUI** and **gpui-kit**.

- **Design:** [docs/](docs/README.md) describes the finished product.
- **Build order:** [IMPLEMENTATION.md](IMPLEMENTATION.md) tracks milestones and status.
- **Brand:** [branding/](branding/README.md).

## Building (development, macOS)

Requirements: Rust (stable), Xcode, and **OBS Studio 33.0.0** installed in `/Applications` (development builds link its `libobs.framework`; set `OBS_APP` to use another location).

```bash
git submodule update --init --depth 1
cargo build
```

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE). Lightair is built on libobs (GPL-2.0-or-later) and GPUI/gpui-kit (Apache-2.0); see [docs/15](docs/15-platform-distribution-licensing.md#licensing).
