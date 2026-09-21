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
- **Middle-click**: Toggle mute
- **Right-click**: Open the context menu
  - Volume adjustment (0–200%)
  - Toggle mute
  - Toggle fullscreen
  - **表示 (View)** — submenu
    - Toggle aspect ratio preservation
    - Toggle always-on-top
    - Toggle the stats overlay (shows FPS and more over the video)
    - Toggle the title bar (borderless mode)
  - **ウィンドウ (Window)** — submenu
    - Toggle window dragging
    - Reset the window size (back to the default 1280x720)
  - Reconnect device
  - Toggle automatic device reconnection
  - Advanced settings
  - Quit

  The toggles live in the 表示 (View) and ウィンドウ (Window) submenus. The flat menu had grown tall enough that on low resolutions — or with the window near the bottom of the screen — the lower entries ended up off-screen and could not be clicked. The menu is now pushed back inside the screen when it would overflow, and scrolls when it still does not fit.
- **Mouse wheel**: Adjust volume (±10%). The current volume appears as a bar at the bottom of the screen and fades out after about 1.5 seconds (the same overlay appears when you change the volume from the context menu slider or the settings dialog)

#### Borderless mode (hiding the title bar)

You can remove the title bar and the window frame, which helps when the window sits on a second monitor as a sub-window. The state is saved to the settings file and restored on the next launch.

- Toggle it with **表示 (View) > タイトルバーを隠す (Hide the title bar)** in the context menu.
- **Moving**: drag the video area. Because of that, turning the title bar off also turns 画面ドラッグ移動 (window dragging) on if it was off, and a notice appears at the bottom of the screen for about two seconds. While the title bar is hidden, window dragging cannot be turned off, since it is the only way left to move the window.
- **Resizing**: move the pointer within about 8 px of a window edge — the cursor changes — and drag. The corners resize diagonally.
- **If the window gets too small**: use ウィンドウ (Window) > ウィンドウサイズをリセット (Reset the window size) in the context menu to go back to the default 1280x720.
- **Quitting**: there is no close button, so use 終了 (Quit) in the context menu or `Alt+F4`.
- The item is disabled while fullscreen, which has no decorations to begin with. Leaving fullscreen restores whatever the setting says.

#### Mute

You can silence the output without dropping the volume to 0%. The volume value is kept while muted, so unmuting resumes at the same level. The state is saved to the settings file and restored on the next launch.

- Toggle it with **middle-click**, the **ミュート (Mute)** checkbox in the context menu, or the **ミュート切替 (Toggle mute)** hotkey.
- Toggling shows ミュート (Muted) / ミュート解除（音量: N%）(Unmuted, volume: N%) at the bottom of the screen for about 1.5 seconds.
- **Turning the mouse wheel (or pressing a volume hotkey) while muted unmutes and applies the volume change.** Those gestures give no on-screen hint that the output is muted, so keeping the mute would look like "I raised the volume but nothing plays".
- Moving the context menu slider does *not* unmute, because the mute checkbox is visible right below it. In that case the volume bar is drawn in grey and reads 音量: N%（ミュート中）(Volume: N%, muted).

### Hotkeys

The settings window → **ホットキー (Hotkeys) tab** lets you assign a key to each of the following actions. **They work while other applications have focus.**

| Action | Description | Default |
|---|---|---|
| スクリーンショット (Screenshot) | Saves the video frame currently on screen | F5 |
| フルスクリーン切替 (Toggle fullscreen) | Switches between fullscreen and windowed | unassigned |
| 最前面表示の切替 (Toggle always-on-top) | Toggles keeping the window above the others | unassigned |
| デバイス再接続 (Reconnect devices) | Reopens the video and audio devices | unassigned |
| 音量を上げる (Volume up) | Raises the volume by 10% (up to 200%) | unassigned |
| 音量を下げる (Volume down) | Lowers the volume by 10% | unassigned |
| ミュート切替 (Toggle mute) | Toggles mute on and off | unassigned |

- Press 設定... (Set) on a row to open the capture dialog. **It accepts a key the instant it opens: press anything other than a modifier key and it captures that combination and closes automatically.** There is no "start capturing" or OK button anymore. Use クリア (Clear) in the list to remove an assignment.
- Global hotkeys are temporarily suspended while the dialog is open, so **you can capture a key that is already assigned to another action** — including reassigning the same key to the action you're currently editing.
- If the key you pressed cannot be accepted, the dialog stays open and shows the reason in red: it was only modifier keys, it's already assigned to another action, or it conflicts with another application and could not be registered.
- Changes take effect when you press 適用 (Apply) or OK.
- A hotkey does exactly what the mouse does: changing the volume shows the volume bar at the bottom, toggling fullscreen shows the indicator at the top-left.
- Assigning the same key to more than one action shows a warning. If you apply it anyway, only the action higher in the list stays active.
- Keys already taken by another application cannot be registered. A notification appears and the reason is also listed below the table. Closing the other application makes the key work again without any action on your part.
- Only the screenshot action has a key by default. Global hotkeys take a key away from every other application, so the rest are left unassigned.

### Stats overlay

Turning on 表示 (View) > "情報表示" (Show stats) in the context menu overlays the following on the top-left of the video. The on/off state is saved to the configuration file and restored on the next launch.

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
3. Configure the destination, save location, file format, and sound effect in the **screenshot settings** tab.
4. Configure the per-action hotkeys in the **ホットキー (Hotkeys)** tab.
5. Export, import or reset the settings in the **その他 (Other)** tab.

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

### Exporting, importing and resetting settings

These live in the **その他 (Other)** tab of the settings window. Use them when moving to another PC, backing up your settings, or attaching your configuration to a bug report.

| Button | Behavior |
|---|---|
| 設定を書き出す... (Export settings) | Saves the current settings as a TOML file. The default file name is `capturecard_viewer-settings-YYYYMMDD.toml` |
| 設定を読み込む... (Import settings) | Reads an exported file into the settings you are currently editing |
| 設定を初期化... (Reset settings) | Resets the settings you are editing to their defaults. It covers the same items as import, so the window position and size are left alone. Nothing happens until you press the confirmation button |

- **Export writes the running settings, not the draft.** Press 適用 first if you want your current edits included.
- **Import and reset only change the draft.** Like every other edit, they reach the running application when you press 適用 or OK, and キャンセル discards them.
- **Window position and size are never imported.** A file exported on a machine with a different display layout will not move your window off-screen. Reset leaves the position and size alone as well.
- Of the items toggled from the right-click menu, always on top, drag to move, stats overlay, mute and borderless mode are not imported. They cannot be changed from the settings window, so importing them would have no effect. Automatic device reconnection *is* imported.
- If a value cannot be understood, only that item falls back to its default and the rest is imported. If the file is not valid TOML, nothing changes and the reason is shown.

### Screenshots

- **Default key**: F5 (configurable; see "Hotkeys")
- **Destination**: Save to a file (default) / copy to the clipboard / both
- **Save location**: Desktop (configurable)
- **File format**: JPEG (default, quality 1-100 selectable, 90 by default) or PNG
- **File name**: `YYYY-MM-DD_HH-MM-SS-mmm.jpg` (`.png` when PNG is selected)
- **Sound effect**: Custom audio files are supported, with adjustable volume

JPEG keeps files small but blurs text and thin lines. Choose PNG when you want to keep game UI or subtitles exactly as rendered; PNG is lossless but produces files several times larger.

Setting the destination to the clipboard (or to both) puts the captured frame straight onto the clipboard, ready to paste into Discord or a chat window. The clipboard copy is uncompressed, so the file format and JPEG quality settings apply only to the file that is written. When only the clipboard is selected, no file is created.

## Where settings are stored

Settings are saved in the following directory:

> %AppData%\capturecard_viewer

Deleting it will recreate the settings with default values on the next launch. 設定を初期化... (Reset settings) in the **その他 (Other)** tab does the same thing, except that the window position and size are kept.

## Recommended settings

The video format, resolution and frame rate options are read from the connected device. The query runs in the background at startup, so the options are usually ready by the time you open the settings window. If it has not finished yet, a spinner appears below the device name and the options fill in once the query completes.

If the query fails, the reason and a "再取得" (retry) button are shown and the options fall back to a built-in default list.

**Video**

- Format: YUY2
- Frame rate: 60 fps
- Color space (色空間): Auto (decided from the resolution)
- Color range (色レンジ): Limited (16-235)

Only change the color space and range when the picture looks wrong. Both take effect on the next frame; the device is not reopened.

Both settings describe **what the incoming signal is**. When they disagree with the actual signal, the picture looks like this:

- **Color space** — leave it on auto unless colors look off. Auto uses BT.709 when `width >= 1280 or height >= 720`, and BT.601 when both are below that. If **skin tones or reds look shifted**, the guess is probably wrong for your device, so pick BT.601 or BT.709 by hand.
- **Color range** — pick "リミテッド" (limited, 16-235) when **blacks look washed out grey and whites look dull**, and "フル" (full, 0-255) when **shadows are crushed and highlights are blown out**. This mirrors the RGB range setting on your capture card or source device.

**Picture adjustments (映像調整): brightness / contrast / saturation**

The same video section has three sliders, each from -100 to 100 with **0 meaning no adjustment**. The "リセット" (reset) button puts all three back to 0.

They are meant for per-device quirks that remain after the color space and range are correct — a picture that is simply too dark or too washed out. **Get the color space and range right first**; covering up a wrong signal interpretation with these sliders only shifts other colors.

- **明るさ (brightness)** — raises (+) or lowers (-) the whole picture
- **コントラスト (contrast)** — widens (+) or narrows (-) the gap between dark and bright. -100 gives a flat mid grey
- **彩度 (saturation)** — strengthens (+) or weakens (-) the colors. -100 gives black and white

The adjustments are folded into the YUY2 -> RGB coefficients, so **the CPU cost barely changes**. The per-pixel work is exactly the same as with no adjustment; what is added is a handful of coefficient multiplications once per frame (no measurable difference at 1080p). Like the color space and range, they take effect on the next frame and the device is not reopened.

> **The color space, color range and picture adjustments can all be inactive.**
> They work by swapping the coefficients used when this app converts YUY2 frames itself. If the device delivers something other than YUY2 (MJPEG, for example), the conversion is left to the decoder and none of these settings apply. When that happens the log contains a line about falling back to the decoder.

**Audio**

- Sample rate: 48000 Hz (chosen from the values the devices support)
- Channels: 2
- Audio passthrough: enabled

The sample rate and channel options only list values that **both the input and the output device support**. As with video, the capabilities are queried on a worker thread, so switching devices does not freeze the window. A spinner is shown while the query runs; if it fails, the reason and a 再取得 (retry) button appear and the options fall back to a fixed list.

- In Windows shared mode the channel count is fixed to the device mix format. When only one value is available the combo box is disabled and the reason is shown.
- When the input and the output have no value in common (for example 48000 Hz input and 44100 Hz output), both sets are listed together with a warning. In that combination the sample rate is converted during playback, and mono/stereo is up- or down-mixed as needed. Playback speed and pitch stay correct, but the conversion costs a little quality, so matching values are still preferable.
- If the saved value is not in the list (after hand-editing the config file, for instance), a warning names the value that is actually used.

What was actually opened is shown under 設定 (Settings) → 接続状態 (Connection status).

## Known issues

Only issues the author is aware of are listed here. See [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) for a fuller list along with workarounds.

**Screenshot hotkey**

- F12 cannot currently be used for screenshots. Registration fails due to a conflict with other software on the system. When registration fails, the reason is shown right in the hotkey capture dialog; if an already-applied key later conflicts, the reason is also shown under the hotkey list in the settings window.

**Device connection at startup**

- If the device is not found, the application keeps retrying until it connects. The interval starts at 0.2 s and widens up to 5 s.
  - This also covers plugging the capture card in after the application has started; just wait and it will connect.
  - Retrying no longer freezes the window for seconds at a time. To retry right away, use Right-click → "デバイス再接続" (Reconnect device).
- The reason for the failure is shown on screen. It appears at the bottom centre for a few seconds, and stays as a second line under the placeholder for as long as there is no video.
  - While the same reason keeps repeating, the bottom-centre message is throttled to at most once every 60 seconds.
  - What the application is actually connected to (the resolution, format, sample rate and channel count it really opened) and the most recent error are always available under 設定 (Settings) → 接続状態 (Connection status).

**Devices that disappear while running**

- When the video stops for 3 seconds, the stale frame is dropped from the screen and the device is reopened. Plugging the USB cable back in restores video and audio without any interaction (it can take up to about 5 seconds).
- Audio disconnection is detected through errors on the input/output streams.
- The on-screen message tells the two cases apart.
  - **映像信号がありません** (No video signal) — the device is still open. Check the power and the HDMI cable of the source.
  - **デバイスが接続されていません（再接続を試しています）** (Device is not connected; reconnecting) — the device itself is gone. Check the USB connection.
- This behaviour can be turned off with Right-click → "デバイスの自動再接続" (Automatic device reconnection). Even when it is off, a stale frame is never left on screen; only the reopening is skipped.

**Settings that are not yet implemented**

Some options can be changed in the settings window but have no effect yet. These are tracked as known issues.

- Selecting MJPEG or RGB24 as the video format (YUY2 is always used internally)

## Reporting problems

Bug reports are welcome via [Issues](https://github.com/Mui-MuiMui/Capturecard_Viewer/issues), in either English or Japanese. Please check [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) first — it lists known issues along with workarounds.

This application depends heavily on the capture card and audio devices you are using, and the author has access to only a limited set of hardware. Please fill in as much of the issue template as you can; without that information, reproducing the problem is usually not possible.

This project is not currently looking for contributors. The conventions the project follows — building, verifying, branching, versioning and the changelog — are written up in [CONTRIBUTING.md](CONTRIBUTING.md) (in Japanese).

## Documentation

The documents below are written in Japanese.

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — the architecture this project is working toward, and how the current code differs from it
- [docs/BUILD.md](docs/BUILD.md) — build instructions and required tooling
- [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md) — dependency status and upgrade plan
- [docs/MANUAL-TEST.md](docs/MANUAL-TEST.md) — manual test checklist
- [CHANGELOG.md](CHANGELOG.md) — release history
- [CONTRIBUTING.md](CONTRIBUTING.md) — where each document lives, and the conventions for building, branching, versioning and the changelog

## Buy me a coffee

If you find this useful, consider buying me a coffee.

<a href='https://ko-fi.com/G2G71JGGSM' target='_blank'><img height='36' style='border:0px;height:36px;' src='https://storage.ko-fi.com/cdn/kofi1.png?v=6' border='0' alt='Buy Me a Coffee at ko-fi.com' /></a>
