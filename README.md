# Dynamic Island for Hyprland

A lightweight, spring-animated Dynamic Island for Hyprland, written in Rust.

Features a minimal floating pill with three interactive views: music playback, system information, and system tray. Supports smooth expand/collapse animations, gesture-based interaction, media controls, and volume adjustment - all rendered on a single transparent surface.

No web stack, no unnecessary layers. Just a little island floating on your desktop. 🌚

## Build

### Dependencies

Install the native development libraries required to build Hyprisland.

**Ubuntu / Debian**

```sh
sudo apt update
sudo apt install -y \
    build-essential \
    pkg-config \
    libgtk-3-dev \
    libgtk-layer-shell-dev \
    libcairo2-dev \
    libpango1.0-dev \
    libgdk-pixbuf-2.0-dev \
    libglib2.0-dev \
    libdbus-1-dev
```

**Fedora**

```sh
sudo dnf install -y \
    gcc \
    gcc-c++ \
    make \
    pkgconf-pkg-config \
    gtk3-devel \
    gtk-layer-shell-devel \
    cairo-devel \
    pango-devel \
    gdk-pixbuf2-devel \
    glib2-devel \
    dbus-devel
```

You also need the stable Rust toolchain and Cargo. Install them using [rustup](https://rustup.rs/) if you don't have them already.

### Build and run

```sh
cargo build --release
./target/release/hyprisland
```

The compiled binary is available at `target/release/hyprisland`.

### Runtime requirements

Hyprisland requires:

- Hyprland
- GTK 3 and gtk-layer-shell
- PipeWire for audio visualization
- The corresponding native runtime libraries

Make sure PipeWire is running and audio capture is available for the visualizer to work.
