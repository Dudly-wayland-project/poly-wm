# Poly

**A Wayland compositor/window manager written in Rust + [Smithay](https://github.com/Smithay/smithay): layouts are done in Lua, still a bit rough around the edges.**

## Documentation

- [Building from source](docs/building.md)
- [Configuration reference](docs/config.md)
- [IPC protocol](docs/ipc.md)
- [Contributor and coding-agent guide](AGENTS.md)

## License

**GPL-3.0-or-later** (see [LICENSE](LICENSE)).

Some modules were ported/adapted from [niri](https://github.com/YaLTeR/niri) (also GPL-3.0-or-later), 
with copyright headers kept in the original files and sources recorded per file in [NOTICE](NOTICE); 
the blur and shader designs were inspired by [Hyprland](https://github.com/hyprwm/Hyprland) (BSD-3-Clause), 
and the noise hash function comes from David Hoskins' `poly_hash12` (MIT, see NOTICE).
