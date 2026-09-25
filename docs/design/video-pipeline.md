# 映像パイプラインと再描画

キャプチャしたフレームが画面に出るまでの経路と、そこで複製を避けるための仕掛け。
色変換の係数へ映像調整を畳み込んでいる理由、再描画をいつ要求するかの判断もここ。
目指す構造は `docs/ARCHITECTURE.md` の「映像パイプライン」にある。

## 映像パイプライン

キャプチャーデバイス → nokhwa `Buffer` → フレームコールバックで YUY2→RGB 変換 → `FrameBuffer` → `update_video_texture` で egui テクスチャ化 → 描画

フレームコールバックの本体（YUY2→RGB 変換、`FrameBuffer` への格納、`RepaintWaker` で UI を起こす）は `video/frame_sink.rs` の `FrameSink` にある。nokhwa のコールバックは `Buffer` から幅・高さ・バイト列を取り出して渡すだけで、フェイクの映像デバイス（`video/fake.rs`）と DirectShow のデバイス（`video/directshow/`、自前のレンダラーの `Receive`）も同じ `FrameSink` を通る。DirectShow の経路だけは YUY2 以外の形式も受ける（`docs/design/device-worker.md` の「DirectShow のバックエンド（#143）」）。

### `FrameSink` が受け取れる形式

| 形式 | 受け口 | 変換（`video/convert.rs`） | 係数表（色空間・レンジ・映像調整） | 届く経路 | DirectShow のサブタイプ |
|---|---|---|---|---|---|
| YUY2 | `push_yuy2` | `yuy2_to_rgb_naive` | 効く | Media Foundation・DirectShow・フェイク | `MEDIASUBTYPE_YUY2` / `MEDIASUBTYPE_YUYV` |
| NV12 | `push_yuv420` | `yuv420_to_rgb` | 効く | DirectShow | `MEDIASUBTYPE_NV12` |
| I420 | `push_yuv420` | `yuv420_to_rgb` | 効く | DirectShow | `MEDIASUBTYPE_I420` / `MEDIASUBTYPE_IYUV` |
| MJPEG | `push_mjpeg` | `mjpeg_to_rgb` | 効かない（デコーダ任せ） | DirectShow | `MEDIASUBTYPE_MJPG` |
| RGB24 | `push_bgr24` | `bgr24_to_rgb` | 効かない（RGB のまま） | DirectShow | `MEDIASUBTYPE_RGB24` |
| そのほか | `push_decoded` | nokhwa のデコーダ | 効かない（デコーダ任せ） | Media Foundation | — |

- **NV12 / I420 は YUY2 と同じ係数表と同じ 1 画素の式を通る。** 同じ Y・Cb・Cr なら YUY2 と同じ RGB になる（`convert.rs` のテストでカラーバーを突き合わせている）。違うのは色差の置き場所だけで、4:2:0 なので縦横 2x2 画素が 1 組の色差を共有する。統計でも高速パスとして数える
- 幅か高さが奇数なら、色差は切り上げた大きさを持つものとして読む（右端の列・下端の行は 1 画素で 1 組）。YUY2 の奇数幅は最後の 1 画素を黒で残すが、4:2:0 は全画素を変換する
- 各面の行に詰め物（ストライドの余り）は無いものとして読む。足りないフレームは捨てる
- DirectShow で形式が未指定のときは、この表の上から順（YUY2・NV12・I420・MJPEG・RGB24）に選ぶ。係数表の効く形式を先にしてある（`SampleKind` の並び）
- YV12（I420 の U と V が逆）は受け取らない

`FrameBuffer` はフレームを `Arc<VideoFrame>` で保持し、取り出し側へは `Arc` の複製を渡す。**画素データを複製しないので、取り出しても 1080p で 6MB の memcpy は発生しない。**

色変換の係数は `ColorMatrix` の表で持つ。**色空間・レンジの選択に加えて、明るさ・コントラスト・彩度もこの表へ畳み込む**（`adjusted_color_matrix`）。Y'CbCr → RGB がアフィン変換であることを使い、コントラストは輝度と色差の係数へ、彩度は色差の係数へ、明るさとコントラストの定数分は `ColorMatrix::offset` へ落とす。変換関数の入口で `y_offset` と `offset` を 1 つの `bias` に畳むので、**調整の有無で 1 画素あたりの演算数は変わらない。** 調整を後段のフィルタとして足さないこと。

色空間・レンジ・映像調整はすべて `SharedColorConversion`（`AtomicU8` ×2 と `AtomicI32` ×3）でフレームコールバックスレッドと共有する。**デバイスを開き直さずに次のフレームから効く。** ここに項目を足すときも `Mutex` にしないこと。フレームコールバックが毎フレーム読む。

`FrameBuffer` は push のたびに進む世代番号を持つ。`update_video_texture` は `VideoFrames::newer_than` で前回反映した世代と比較し、新着が無ければテクスチャを更新しない。**新着の有無を問わず最後のフレームが要る用途（スクリーンショット）は `VideoFrames::latest` を使う。** 世代番号はキャプチャ停止時も巻き戻さない。巻き戻すと再接続後の最初のフレームが呼び出し側の記録と一致し、新着と判別できなくなる。

## 再描画をいつ要求するか

eframe は **`update()` の中で要求された再描画しか予約しない。** 何も要求しなければ次の `update()` は来ないので、時間で動くもの（フレームの途絶の判定、接続の再試行、OSD の消滅、設定の遅延書き出し）は自分の都合で予約する必要がある。判断は `src/repaint.rs` に集めてある。

`update()` の末尾で `next_repaint_delay` が「次の `update()` までに空けてよい時間」を 1 回だけ要求する。

| 状態 | 間隔 |
|---|---|
| 最小化している | 1 秒 |
| 最後のフレームから 200ms 以内 | 16ms（60fps） |
| それ以外（信号なし、デバイスなし） | 250ms |

- **ここは上限であって下限ではない。** `overlay.rs` の OSD（消える時刻）、`flush_settings_if_due`（書き出す時刻）はそれぞれ自分で予約している。egui の `request_repaint_after` は同じフレームで要求された中の最短を採るので、互いに邪魔しない
- 以前は `update_video_texture` が無条件に 16ms を予約しており、映像が来ていなくても 60fps で描き続けていた（Issue #98）。**映像の有無にかかわらず一定間隔で予約する形へ戻さないこと**

映像が来ていないときは `RepaintWaker`（`repaint.rs`）が UI スレッド以外からの再描画を引き受ける。`egui::Context` を 1 つだけ持ち、フレームコールバックスレッドとホットキーのリスナースレッドが複製を握っている。これがあるので、250ms へ広げていても映像やホットキーはその場で反応する。

- **起こすのは間隔を広げているときだけ**（`should_wake_on_event`）。16ms で回っている間に起こすと、描いている最中に届いた通知が「新着なし」の `update()` を増やし、**1080p60 の実測で CPU が 66% → 85%（1 コアあたり）に増えた**
- **最小化中も起こさない。** eframe は最小化されたウィンドウの再描画要求を捨てる（`eframe::native::run` の `windows_next_repaint_times`）ため、起こしてもイベントループが動くだけで何も描かれない。**最小化中は `update()` 自体が呼ばれない**ので、そこで動く処理も止まる。接続の再試行と切断の監視はデバイスワーカースレッドへ移したので影響を受けない（#133）。ホットキーのアクションも、画面が要らないもの（デバイス再接続・音量・ミュート）はリスナースレッドからワーカーへ流して実行する。画面が要るもの（フルスクリーン切替など）だけが復帰まで持ち越される（`docs/design/hotkeys.md` の「最小化中の扱い」）
- 広げる判断に 200ms の猶予を置いてあるのは、広げた直後に次のフレームが届くと `RepaintWaker` を有効にするより先に到着してしまい、その 1 枚が 250ms 遅れて出るため
- `Context` と結びつくのは最初の `update()`（`RepaintWaker::bind`）。**`RepaintWaker` は `VideoCapture::new` の引数で渡す。** フレームコールバックは `start_capture` の時点の複製を持つので、開いたあとに差し替える手段は用意していない
- 渡し忘れても映像は止まらない。既定の `RepaintWaker` は何もせず、ポーリングだけで動き続ける
## UI にあるが動作していない設定がある

以下は設定画面から変更できるが実装が追いついていない。README の記述もこれらを前提に書かれているため、修正時は README も合わせて更新すること。

- ビデオフォーマットの MJPEG / RGB24（Media Foundation の経路では内部で YUYV に強制される。DirectShow の経路〈「(DirectShow)」のデバイス〉では選んだ形式で開く）

オーディオのサンプリングレート／チャンネル数は `select_best_config` でストリームに反映され、**選択肢も入出力デバイスの対応設定から生成している**（`audio::selectable_sample_rates` / `selectable_channels`）。デバイスの能力を取得できなかった場合だけ固定の既定一覧へ倒すので、そのときは対応しない値も選べる。
