[日本語](README.md) | **English**

# Capturecard Viewer

A viewer for displaying video and audio from a capture card. For Windows 10/11.

## Overview

- This application displays video and audio from a capture card (capture board) with low latency, good image quality, and a minimal interface.
- Recent capture cards (as of 2025) should work, but older ones may not. The author has no way to verify this, as those devices are not available for testing.
  - More precisely, it should work with any device that Windows recognizes as a webcam.
- AI is used in parts of this project's development. If you would rather not use software developed this way, please do not use it.

## Installation

Save it anywhere and run it.

## Uninstallation

Delete the folder.

To also remove the settings file, delete the following directory:

> %AppData%\Capturecard_Viewer

## Usage

### Basic controls

- **Double-click**: Toggle fullscreen
- **Drag**: Drag the video area to move the window
- **Right-click**: Open the context menu
  - Volume adjustment (0–200%)
  - Toggle aspect ratio preservation
  - Toggle always-on-top
  - Toggle fullscreen
  - Toggle window dragging
  - Reconnect device
  - Advanced settings
- **Mouse wheel**: Adjust volume (±10%)

### Settings

1. Right-click → "詳細設定..." (Advanced settings) to open the settings window.
2. Select video and audio devices in the **device settings** tab.
    - Changes take effect when you press 適用 (Apply) or OK. Settings are saved to disk only when you press OK.
    - The device list is cached and refreshed every 5 seconds.
3. Configure the save location, sound effect, and hotkey in the **screenshot settings** tab.

> **Note:** The application interface is currently Japanese only.

### Screenshots

- **Default key**: F5 (configurable)
- **Save location**: Desktop (configurable)
- **File name**: `YYYY-MM-DD_HH-MM-SS-mmm.jpg`
- **Sound effect**: Custom audio files are supported, with adjustable volume

## Where settings are stored

Settings are saved in the following directory:

> %AppData%\Capturecard_Viewer

Deleting it will recreate the settings with default values on the next launch.

## Recommended settings

Video settings are read from the device, so some options may not appear.

**Video**

- Format: YUY2
- Frame rate: 60 fps

**Audio**

- Sample rate: 32000 Hz (a matter of preference)
- Channels: 2
- Audio passthrough: enabled

## Known issues

Only issues the author is aware of are listed here. See [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) for a fuller list along with workarounds.

**Screenshot hotkey**

- F12 cannot currently be used for screenshots. Registration fails due to a conflict with other software on the system.

**Device connection at startup**

- The application sometimes fails to connect to the device at startup.
  - Use Right-click → "デバイス再接続" (Reconnect device) to work around this.

**Settings that are not yet implemented**

Some options can be changed in the settings window but have no effect yet. These are tracked as known issues.

- Enabling or disabling audio passthrough
- Audio sample rate and channel count
- Selecting MJPEG or RGB24 as the video format (YUY2 is always used internally)

## Reporting problems

Bug reports are welcome via [Issues](https://github.com/Mui-MuiMui/Capturecard_Viewer/issues), in either English or Japanese. Please check [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) first — it lists known issues along with workarounds.

This application depends heavily on the capture card and audio devices you are using, and the author has access to only a limited set of hardware. Please fill in as much of the issue template as you can; without that information, reproducing the problem is usually not possible.

This project is not currently looking for contributors.

## Documentation

The documents below are written in Japanese.

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — the architecture this project is working toward, and how the current code differs from it
- [docs/BUILD.md](docs/BUILD.md) — build instructions and required tooling
- [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md) — dependency status and upgrade plan
- [docs/MANUAL-TEST.md](docs/MANUAL-TEST.md) — manual test checklist
- [CHANGELOG.md](CHANGELOG.md) — release history

## Buy me a coffee

If you find this useful, consider buying me a coffee.

<a href='https://ko-fi.com/G2G71JGGSM' target='_blank'><img height='36' style='border:0px;height:36px;' src='https://storage.ko-fi.com/cdn/kofi1.png?v=6' border='0' alt='Buy Me a Coffee at ko-fi.com' /></a>
