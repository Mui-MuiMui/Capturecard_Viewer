# 映像遅延の実測

**1080p60、Live Gamer EXTREME 3、Media Foundation の条件で、パススルーの表示に対して約 38ms（33〜42ms）の遅れ。**

2026-10-04 に測った（Issue #453）。**この条件での値**であり、他のボード・解像度・開き方・モニターには当てはまらない。同じ機材で他のアプリも測ったところ、AVerMedia RECentral 4 が約 18ms、アマレコTV 3.10 が約 28ms で、**本アプリが 3 つの中で最も遅い**（「他のアプリとの比較」）。

## 結果

| 項目 | 値 |
|---|---|
| パススルーの表示に対するアプリの表示の遅れ | 約 38ms（平均 9.2 カメラフレーム） |
| 最小 / 最大 | 33ms / 42ms（8 / 10 カメラフレーム） |
| 読み取りの誤差 | ±4ms（±1 カメラフレーム） |

## 条件

| 項目 | 内容 |
|---|---|
| アプリ | Capturecard Viewer 1.3.0 |
| 映像の開き方 | Media Foundation（このボードは Media Foundation でしか開けない） |
| 解像度と fps、形式 | 1920x1080 60fps、YUY2 |
| キャプチャーボード | AVerMedia Live Gamer EXTREME 3（USB） |
| 信号源 | 別 PC で `scripts/make-testpattern.sh` のテスト動画（1080p60、フレーム番号を焼き込み）を全画面再生 |
| モニター | MSI MAG 244F（200Hz）× 2 台。リフレッシュレート・応答速度・画面モードを同じ設定に揃えた |
| 撮影 | スマートフォン、FHD 240fps のスローモーション（1/8 倍速で保存、1 カメラフレーム = 4.17ms）。手持ち |
| 撮影した動画 | 84 秒、2533 フレーム |

## 測り方

```mermaid
flowchart LR
    PC["信号源の PC<br>テスト動画を再生"] -->|HDMI| B[Live Gamer EXTREME 3]
    B -->|HDMI パススルー| L[左のモニター]
    B -->|USB| A[Capturecard Viewer]
    A --> R[右のモニター]
    L --> C["スマートフォン<br>240fps で撮影"]
    R --> C
```

1. 信号源の PC で、フレーム番号を焼き込んだ 1080p60 のテスト動画（`scripts/make-testpattern.sh` で作る）を全画面で再生する
2. 同じ型番のモニターを 2 台並べ、同じ設定にする。左にボードの HDMI パススルー、右にアプリの表示を出す
3. スマートフォンの 240fps のスローモーションで、2 台が 1 枚に収まるように撮る

モニターが 200Hz なので表示側の刻みは 5ms で、ソースの 60fps（16.7ms 刻み）より細かい。そのため遅れはフレーム番号の差で読める。パススルー側も同じ型番・同じ設定のモニターなので、モニターの表示遅延は左右で等しいとみなして差し引いている。2 台の個体差は評価していない。

## 解析の手順

ffmpeg 9.0.2 で撮影した動画から静止画を切り出し、番号は画像を目で見て読んだ。crop の座標は撮影ごとに合わせる。

### 1. 2 秒おきの 40 枚

ファイル時刻で 2 秒おき（実時間 0.25 秒おき）に 1 枚ずつ、左右の番号の部分だけを並べて切り出す。

```bash
ffmpeg -ss <t> -i <動画> -frames:v 1 \
  -vf "split[a][b];[a]crop=360:140:360:430[l];[b]crop=360:140:1280:430[r];[l][r]hstack" pair_NN.png
```

できた 40 枚を `tile=2x10` で一覧にして読む。40 枚すべてで 左 − 右 = 2 だった。60fps の番号の差なので 33.3ms 相当だが、番号の刻みは 16.7ms なので真の値は 1.x〜2.x フレームの幅にある。これだけでは細かく決まらないので、次の方法で詰める。

### 2. 連続区間 3 つ

連続 48 カメラフレーム（実時間 0.2 秒）を、ファイル時刻 10s / 40s / 70s の 3 区間で切り出す。

```bash
ffmpeg -ss <t> -i <動画> -frames:v 48 \
  -vf "split[a][b];[a]crop=300:120:400:440[l];[b]crop=300:120:1320:440[r];[l][r]hstack,drawtext=text='%{n}',tile=4x12" \
  -frames:v 1 burst_<t>.png
```

同じ番号 V が左に現れたカメラフレームと、右に現れたカメラフレームの番号の差を取る。

| 区間 | ファイル時刻 | 番号 | 差の最小 | 差の最大 | 差の平均 |
|---|---|---|---|---|---|
| 1 | 10s | 219〜230 | 8 | 10 | 9.1 |
| 2 | 40s | 447〜455 | 8 | 10 | 9.2 |
| 3 | 70s | 672〜680 | 8 | 10 | 9.2 |

差 9.2 カメラフレーム × 4.17ms = 約 38ms。左（パススルー）は 4 カメラフレームごと（16.7ms）に規則正しく進み、右（アプリ）は 3〜5 カメラフレームの間隔で揺れる。200Hz の描画と 60fps の到着の噛み合いによる。

## 他のアプリとの比較

同じ日に同じ機材・同じ信号源で、右のモニターに出すアプリだけを替えて撮り、同じ手順で読んだ（Issue #453）。値はどれもパススルーの表示に対する遅れ。

| アプリ | 映像の経路 | 遅れ | 最小 / 最大 | 2 秒おき 40 枚の 左 − 右 |
|---|---|---|---|---|
| AVerMedia RECentral 4 | 不明（ボードの純正アプリ） | 約 18ms（4.3 カメラフレーム） | 13ms / 21ms | 1 が 36 枚、0 が 1 枚、2 が 3 枚 |
| アマレコTV 3.10 | DirectShow | 約 28ms（6.8 カメラフレーム） | 21ms / 33ms | 1 が 30 枚、2 が 10 枚 |
| Capturecard Viewer 1.3.0 | Media Foundation | 約 38ms（9.2 カメラフレーム） | 33ms / 42ms | 2 が 40 枚 |

連続区間 3 つの差（カメラフレーム）:

| アプリ | 区間 1（10s） | 区間 2（40s） | 区間 3（70s） |
|---|---|---|---|
| RECentral 4 | 番号 760〜771、差 3〜5、平均 4.0 | 番号 985〜997、差 4〜5、平均 4.7 | 番号 1209〜1221、差 4〜5、平均 4.1 |
| アマレコTV 3.10 | 番号 347〜360、差 6〜7、平均 6.7 | 番号 572〜585、差 5〜8、平均 6.5 | 番号 796〜810、差 6〜8、平均 7.1 |

読み取れたこと:

- 3 つの中では本アプリが最も遅い。RECentral 4 との差は約 20ms（ソースの 1 フレーム強）、アマレコTV との差は約 10ms
- RECentral 4 の表示は、パススルーと同じ 4 カメラフレームの刻みで、パススルーをちょうど 1 刻み（16.7ms）ずらした形で進む。届いたフレームを次の表示にそのまま出していると読める
- アマレコTV の表示は 5〜8 カメラフレームの幅で揺れる。本アプリの 3〜5 カメラフレームの描画間隔の揺れと似た性質

比べるときの注意:

- 他のアプリの設定（表示モード、垂直同期、バッファの有無）は記録していない。ウィンドウの大きさや配置も本アプリと同じではない（切り出した画像の中でパターンの位置が違う）
- 撮影した動画は RECentral 4 が 86 秒 2580 フレーム、アマレコTV が 87 秒 2605 フレーム。crop の座標は本アプリの撮影と違う（RECentral 4 は左 `400:180:260:450`・右 `400:180:1260:460`、アマレコTV は左 `400:180:200:430`・右 `400:180:1240:430`）
- `drawtext` はフォントの設定が無い環境では落ちるので、`fontfile=` でフォントのファイルを渡す

## `MF_LOW_LATENCY` の撮り比べ（#456、効果なし）

Media Foundation のソースリーダーに `MF_LOW_LATENCY = TRUE` を付けた版（PR #461）を、同じ exe で ON と OFF（環境変数で切り替え）にして、上と同じ機材・同じ手順で撮り比べた。どちらの動画もアプリのログで ON / OFF を確かめている。リプレイバッファ（300 秒、NVIDIA の H.264 エンコーダー）は両方で ON。

| 条件 | 2 秒おき 40 枚の 左 − 右 | 連続区間 3 つの差（カメラフレーム） | 遅れ |
|---|---|---|---|
| ON | 2 が 38 枚、3 が 2 枚 | 9.0 / 9.6 / 9.0（8〜10） | 約 38ms |
| OFF | 2 が 39 枚、3 が 1 枚 | 9.3 / 9.0 / 9.0（8.5〜10） | 約 38ms |

差は読み取りの誤差（±1 カメラフレーム）の中で、効果は無かった。この属性はソースリーダーの下のデコーダーや変換の MFT の溜め込みを減らすもので、YUY2 を無変換で受け取る経路には効かないとみられる。変更は同日に戻した（`docs/design/video-pipeline.md` の「試して外したもの」）。

同じ日の実機で、アプリ内の計測（下の「アプリ内の計測の読み方」）は平均 8ms / 最大 11ms だった。約 38ms のうちアプリ内で見える分はこれだけで、残りはボードの取り込みと present・モニターの側にある。

## 注意

- **含まれるもの**: ボードの取り込み（USB、Media Foundation）、アプリの変換と描画
- **含まれないもの**: パススルー自体の遅れ。つまり絶対的な入力遅延ではなく、パススルーとの差。モニターの表示遅延は左右で等しいとみなして差し引いているが、2 台の個体差は評価していないので、その差は値に入りうる
- 読み取りの誤差は ±1 カメラフレーム（±4ms）。手持ちの手ぶれは番号の読み取りに影響しなかった
- **同じ条件でしか比べられない。** ボード、解像度と fps、開き方、モニター、撮影の fps、アプリのバージョン、描画バックエンドのどれかが変われば測り直す
- **描画バックエンド（glow / wgpu）の撮り比べは同じ exe で行う**（#456 の (2)）。既定は wgpu（DX12、Mailbox）で、環境変数 `CAPTURECARD_VIEWER_RENDERER=glow` を付けて起動すると glow になる。コマンドプロンプトなら `set CAPTURECARD_VIEWER_RENDERER=glow` のあとに同じ窓から exe を起動し、wgpu に戻すときは `set CAPTURECARD_VIEWER_RENDERER=` で消してから起動する。どちらで描いたかは統計 OSD の「描画」の行とログで確かめ、撮影ごとに書き残す。GPU の DX12 アダプターが無い環境では既定でも glow になる（`docs/design/video-pipeline.md` の「描画バックエンド」）。比べるのはウィンドウ表示とフルスクリーンの両方で、フルスクリーンでは他のウィンドウや通知が重ならないようにする（independent flip の条件）

## アプリ内の計測の読み方

統計 OSD（右クリックメニューの「情報表示」）と「接続状態」タブの映像の欄に出る「表示までの遅れ: 平均 N ms / 最大 M ms」は、アプリが自分で測った値（#455）で、上の実測とは測っている範囲が違う。起点は Media Foundation（DirectShow なら自前のレンダラー）のコールバックが呼ばれた時刻で、ボードが取り込んだ時刻より後になる。終点は UI スレッドがそのフレームでテクスチャを更新した時刻で、GPU が画面へ出す（present）までと、モニターの表示遅延は含まない。つまり上の約 38ms のうち、アプリの変換と UI スレッドが取り込むまでの待ちの分だけを表す。ボードの取り込みの分は「実測の遅れ − この値 − present とモニターの分」として残る。値は直近 1 秒の平均と最大で、30 秒ごとの集計はログレベルを `debug`（環境変数 `CAPTURECARD_VIEWER_LOG=debug`）にするとログにも出る。

## Summary (English)

Measured on 2026-10-04 (Issue #453): with Capturecard Viewer 1.3.0, an AVerMedia Live Gamer EXTREME 3 opened through Media Foundation at 1920x1080 60fps (YUY2), and two identical MSI MAG 244F monitors at 200Hz, the app's picture lags the card's HDMI passthrough by about 38 ms (33 to 42 ms, reading error ±4 ms). A test video with burned-in frame numbers was played on another PC, both monitors were filmed with a smartphone at 240fps, and the frame numbers were read from stills extracted with ffmpeg (40 stills every 2 seconds, plus three continuous 48-frame bursts). The value includes capture, conversion, and drawing in the app, assumes both monitors have the same display latency (unit-to-unit differences were not evaluated), and applies only to these conditions. On the same day and setup, AVerMedia RECentral 4 lagged the passthrough by about 18 ms (13 to 21 ms) and AmaRecTV 3.10 by about 28 ms (21 to 33 ms), so Capturecard Viewer 1.3.0 was the slowest of the three. The "Display latency" line in the stats overlay and the Status tab (#455) measures a narrower span inside the app: it starts when the Media Foundation (or DirectShow) callback receives the frame, which is after the card has captured it, and ends when the UI thread updates the texture, so it does not include GPU presentation or the monitor. Since #456 (2) the app draws with wgpu on DX12 (flip-model swapchain, Mailbox present mode) by default; set the environment variable `CAPTURECARD_VIEWER_RENDERER=glow` to draw with OpenGL (glow) from the same exe when comparing the two.
