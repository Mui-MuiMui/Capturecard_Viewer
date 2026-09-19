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

> %AppData%\capturecard_viewer

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
  - Toggle the stats overlay (shows FPS and more over the video)
  - Reconnect device
  - Advanced settings
- **Mouse wheel**: Adjust volume (±10%)

### Stats overlay

Turning on "情報表示" (Show stats) in the context menu overlays the following on the top-left of the video. The on/off state is saved to the configuration file and restored on the next launch.

| Item | Description |
|---|---|
| FPS | Effective frame rate derived from the average of the last 120 frame intervals. It is calculated from the frames that actually arrived, not queried from the device |
| ばらつき (Jitter) | Standard deviation, minimum and maximum of the frame intervals. Dropped frames or capture stalls make this grow |
| デコード (Decode) | Time spent on the RGB conversion of the most recent frame, plus how many frames took the fast path versus the generic path |
| Resolution / format | Pixel size and input format of the frames actually arriving |
| 最終フレーム (Last frame) | Time elapsed since the last frame arrived |

### Settings

1. Right-click → "詳細設定..." (Advanced settings) to open the settings window.
2. Select video and audio devices in the **device settings** tab.
    - The device lists (both video and audio) are cached and refreshed every 5 seconds.
3. Configure the save location, file format, sound effect, and hotkey in the **screenshot settings** tab.

Edits in the settings window are kept as a draft and do not affect the running application until you press a button.

| Button | Behavior |
|---|---|
| 適用 (Apply) | Applies the edits to the running application and saves them to the settings file. Keeps the window open |
| OK | Does the same as 適用, and then closes the window |
| キャンセル (Cancel) | Discards edits that have not been applied yet, and closes the window. **Anything already applied with 適用 is NOT reverted** |
| × (title bar) | Same as Cancel |

- The only difference between 適用 and OK is whether the window closes. Both save to the settings file, so the values survive a restart.
- Hotkey and sound file changes also take effect only after you press 適用 or OK. The sound "テスト再生" (test playback) button plays the current sound effect at the volume you are editing.

> **Note:** The application interface is currently Japanese only.

### Screenshots

- **Default key**: F5 (configurable)
- **Save location**: Desktop (configurable)
- **File format**: JPEG (default, quality 1-100 selectable, 90 by default) or PNG
- **File name**: `YYYY-MM-DD_HH-MM-SS-mmm.jpg` (`.png` when PNG is selected)
- **Sound effect**: Custom audio files are supported, with adjustable volume

JPEG keeps files small but blurs text and thin lines. Choose PNG when you want to keep game UI or subtitles exactly as rendered; PNG is lossless but produces files several times larger.

## Where settings are stored

Settings are saved in the following directory:

> %AppData%\capturecard_viewer

Deleting it will recreate the settings with default values on the next launch.

## Recommended settings

The video format, resolution and frame rate options are read from the connected device. The query runs in the background at startup, so the options are usually ready by the time you open the settings window. If it has not finished yet, a spinner appears below the device name and the options fill in once the query completes.

If the query fails, the reason and a "再取得" (retry) button are shown and the options fall back to a built-in default list.

**Video**

- Format: YUY2
- Frame rate: 60 fps

**Audio**

- Sample rate: 32000 Hz (a matter of preference)
- Channels: 2
- Audio passthrough: enabled

The audio options are listed regardless of the device, so you can pick a value your device does not support. In that case the closest value the device does support is used. (Channel count is often limited to the device default.)

## Known issues

Only issues the author is aware of are listed here. See [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) for a fuller list along with workarounds.

**Screenshot hotkey**

- F12 cannot currently be used for screenshots. Registration fails due to a conflict with other software on the system.

**Device connection at startup**

- If the device is not found, the application keeps retrying until it connects. The interval starts at 0.2 s and widens up to 5 s.
  - This also covers plugging the capture card in after the application has started; just wait and it will connect.
  - Retrying no longer freezes the window for seconds at a time. To retry right away, use Right-click → "デバイス再接続" (Reconnect device).

**Settings that are not yet implemented**

Some options can be changed in the settings window but have no effect yet. These are tracked as known issues.

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
