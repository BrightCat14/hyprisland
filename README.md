# Dynamic Island for Hyprland

A lightweight, spring-animated Dynamic Island for Hyprland, rewritten in Rust.

Features a minimal floating pill with three interactive views: music playback, system information, and system tray. Supports smooth expand/collapse animations, gesture-based interaction, media controls, and volume adjustment - all rendered on a single transparent surface.

No web stack, no unnecessary layers. Just a little island floating on your desktop. 🌚

## Build

Requires GTK 3, gtk-layer-shell, and PipeWire.

```sh
cargo build --release
./target/release/hyprisland
```
