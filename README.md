# Jellium Desktop

An unofficial [Jellyfin](https://jellyfin.org) desktop client built on [CEF](https://github.com/chromiumembedded/cef) and [mpv](https://mpv.io/).

## Downloads

This fork builds Windows x64. Linux, macOS, and Windows ARM64 downloads below are from [upstream](https://github.com/andrewrabert/jellium-desktop).

### Linux
- AppImage
  - [x86_64](https://nightly.link/andrewrabert/jellium-desktop/workflows/build-linux-appimage/main/linux-appimage-x86_64.zip)
  - [aarch64](https://nightly.link/andrewrabert/jellium-desktop/workflows/build-linux-appimage/main/linux-appimage-aarch64.zip)
- Arch Linux (AUR): [jellium-desktop-git](https://aur.archlinux.org/packages/jellium-desktop-git)
- [Flatpak (non-Flathub bundle)](https://nightly.link/andrewrabert/jellium-desktop/workflows/build-linux-flatpak/main/linux-flatpak-x86_64.zip)

### macOS
- [Apple Silicon](https://nightly.link/andrewrabert/jellium-desktop/workflows/build-macos/main/macos-arm64.zip)
- [Intel](https://nightly.link/andrewrabert/jellium-desktop/workflows/build-macos/main/macos-x86_64.zip)

After installing, remove quarantine: 
```
sudo xattr -cr /Applications/Jellium\ Desktop.app
```

### Windows

- [x64 ZIP (this fork)](https://nightly.link/phoval/jellium-desktop/workflows/build-windows/main/windows-x64.zip)
- [x64 MSI installer (this fork)](https://nightly.link/phoval/jellium-desktop/workflows/build-windows/main/windows-x64-msi.zip)
- [ARM64 (upstream)](https://nightly.link/andrewrabert/jellium-desktop/workflows/build-windows/main/windows-arm64.zip)

The MSI installs to Program Files and adds Start Menu and desktop shortcuts. It
can be removed through Windows Settings. To omit the desktop shortcut during a
silent installation, pass `DESKTOPSHORTCUT=0` to `msiexec /i <installer.msi> /qn`.

## Development

This project uses [just](https://github.com/casey/just) as a command runner.

### Windows with mise

Install [mise](https://mise.jdx.dev/getting-started.html), Rust via [rustup](https://rustup.rs/), and PowerShell 7. Use either Visual Studio's C++ build tools or an existing [msvcup](https://github.com/marler8997/msvcup) installation with the `autoenv` package.

Run these commands in PowerShell from the repository root:

```powershell
mise install
mise x -- just deps
mise x -- just build
mise x -- just run
```

Mise installs CMake, just, Meson, and Ninja. The Windows configuration loads automatically and uses `C:\msvcup\msvc` by default; set `MSVCUP_ROOT` to the directory containing `autoenv` if your installation is elsewhere. Visual Studio users can build from a Developer PowerShell instead.

`just deps` initializes submodules, downloads CEF, and builds mpv with MSYS2. Run it once before building and again when native dependencies change. Setup installs MSYS2 at `C:\msys64` if needed and installs its required packages. Bindgen uses MSYS2's libclang with the MSVC target; the build script loads msvcup's header and library paths into the build environment.

The runnable application is staged at `build\jellium-desktop.exe`. To run the checks used before committing:

```powershell
mise x -- just fmt
mise x -- just lint
```

Build the Windows installer with `mise x -- just msi -Validate`. This requires a
.NET SDK and installs the pinned WiX 5 tool under `build/installer/tools` on first
use. The MSI is written to `dist/`. Pass `-Scope perUser` to install under
`%LOCALAPPDATA%\Programs` without elevation. The package is unsigned unless
`-SignThumbprint <certificate-thumbprint>` is supplied.

### Recipes

```
Available recipes:
    [deps]
    deps          # Set up platform dependencies

    [package]
    appimage ...    # [linux] build AppImage
    flatpak ...     # [linux] build Flatpak bundle
    dmg             # [macos] build Apple Disk Image (.dmg)
    msi *args       # [windows] build Windows Installer (.msi)

    [maintenance]
    outdated      # List outdated dependencies
    clean         # Remove build artifacts

    [test]
    test          # Run tests

    [lint]
    fmt           # Format workspace
    fmt-check     # Check formatting
    clippy        # Run clippy
    lint          # Lint workspace
    strict-lint   # Strict lint workspace

    [build]
    build         # Build the app

    [run]
    run *args     # Run the app
    run-mpv *args # Run the mpv CLI
```
