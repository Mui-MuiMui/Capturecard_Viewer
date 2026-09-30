#!/usr/bin/env bash
# 実機テスト用の映像源を ffmpeg で作る。キャプチャーボードの HDMI 入力へつなぐ PC でループ再生する。
#
#   scripts/make-testpattern.sh [秒数] [出力ファイル]
#
# 入っているもの:
#   - SMPTE の HD カラーバー（既知の値なので、届いた画素と数値で照合できる）
#   - 中央下に大きなフレーム番号（フレーム落ち・重複、録画の PTS の照合）
#   - 1 秒ごとに 50ms だけ左上に白い四角と 2kHz の合図音（映像と音声のずれを数値で測る）
#     合図音の sine は samples_per_frame=240（5ms）にしてある。volume の eval=frame は音声フレーム単位でしか
#     切り替わらないので、既定の 1024 サンプル（約 21ms）のままだと合図音が四角と最大 18ms ずれる
#   - 左 440Hz、右 1kHz の連続音（チャンネルの入れ替わり、ドリフトの測定）
#
# 1080p60、BT.709 のリミテッドレンジ。ffmpeg は PATH に要る（Windows の winget 版は
# fontconfig を持たないので、フォントは fontfile で直接指定している）。
# ループ再生するとフレーム番号は 0 に戻るが、検証には支障ない。
set -euo pipefail
duration="${1:-1800}"
out="${2:-testpattern_1080p60.mp4}"
font='C\:/Windows/Fonts/consola.ttf'

ffmpeg -hide_banner -y \
  -f lavfi -i "smptehdbars=size=1920x1080:rate=60" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -f lavfi -i "sine=frequency=1000:sample_rate=48000" \
  -f lavfi -i "sine=frequency=2000:sample_rate=48000:samples_per_frame=240" \
  -filter_complex "[0:v]drawtext=text='%{n}':fontfile='${font}':fontsize=140:fontcolor=white:box=1:boxcolor=black:x=(w-text_w)/2:y=h*0.64,drawbox=x=0:y=0:w=240:h=240:color=white:t=fill:enable='lt(mod(t,1),0.05)'[v];[3:a]volume=volume='if(lt(mod(t,1),0.05),1,0)':eval=frame,aformat=channel_layouts=stereo[beep];[1:a][2:a]amerge=inputs=2[lr];[lr][beep]amix=inputs=2:normalize=0[a]" \
  -map "[v]" -map "[a]" -t "${duration}" -r 60 \
  -c:v libx264 -preset veryfast -crf 16 -pix_fmt yuv420p \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
  -movflags +faststart -c:a aac -b:a 192k "${out}"
