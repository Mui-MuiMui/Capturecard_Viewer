# 映像パイプラインと再描画

キャプチャしたフレームが画面に出るまでの経路と、そこで複製を避けるための仕掛け。
色変換の係数へ映像調整を畳み込んでいる理由、再描画をいつ要求するかの判断もここ。
目指す構造は `docs/ARCHITECTURE.md` の「映像パイプライン」にある。

## 映像パイプライン

キャプチャーデバイス → nokhwa `Buffer` → フレームコールバックで YUY2→RGB 変換 → `FrameBuffer` → `update_video_texture` で egui テクスチャ化 → 描画

録画中はここから分岐する: `FrameBuffer` へ置いて `RepaintWaker` で UI を起こした**あとに**、同じ `Arc<VideoFrame>` を `VideoTap` のリングへ積む → 録画スレッドが RGB → NV12 → Sink Writer（H.264 / MP4）。

- **画面へ出す経路の後ろに置き、表示の遅延に足さない。** コールバックが増やすのは `Arc` の複製 1 回と待たない `try_lock` 1 回だけで、画素は複製しない。リングが満杯なら積まずに捨てて数える
- 録画していない間はリングが無く、コールバックは旗（`AtomicBool`）を 1 つ読むだけ
- 変換後の RGB を渡すので、色空間・レンジ・映像調整が乗った画面と同じ見た目で録画される。書き出す NV12 は常に標準の組（HD は BT.709、SD は BT.601、リミテッドレンジ）で、その印を H.264 に付ける
- リングの容量は 3 と小さい。リングの `Arc` が `FrameSink` の Vec の回収を妨げるので、録画スレッドは取り出したらすぐ NV12 へ直して手放す。回収に失敗した回数は録画中だけ数え、止めたときにログへ出す
- 詳しくは `docs/design/recording.md` の「コールバックから渡す経路」

フレームコールバックの本体（YUY2→RGB 変換、`FrameBuffer` への格納、`RepaintWaker` で UI を起こす）は `video/frame_sink.rs` の `FrameSink` にある。nokhwa のコールバックは `Buffer` から幅・高さ・バイト列を取り出して渡すだけで、フェイクの映像デバイス（`video/fake.rs`）と DirectShow のデバイス（`video/directshow/`、自前のレンダラーの `Receive`）も同じ `FrameSink` を通る。DirectShow の経路だけは YUY2 以外の形式も受ける（`docs/design/device-worker.md` の「DirectShow のバックエンド（#143）」）。

### `FrameSink` が受け取れる形式

| 形式 | 受け口 | 変換（`video/convert.rs`、4:2:0 は `video/yuv420.rs`） | 係数表（色空間・レンジ・映像調整） | 届く経路 | DirectShow のサブタイプ |
|---|---|---|---|---|---|
| YUY2 | `push_yuy2` | `yuy2_to_rgb_naive` | 効く | Media Foundation・DirectShow・フェイク | `MEDIASUBTYPE_YUY2` / `MEDIASUBTYPE_YUYV` |
| NV12 | `push_yuv420` | `yuv420_to_rgb` | 効く | DirectShow | `MEDIASUBTYPE_NV12` |
| I420 | `push_yuv420` | `yuv420_to_rgb` | 効く | DirectShow | `MEDIASUBTYPE_I420` / `MEDIASUBTYPE_IYUV` |
| YV12 | `push_yuv420` | `yuv420_to_rgb` | 効く | DirectShow | `MEDIASUBTYPE_YV12` |
| MJPEG | `push_mjpeg` | `mjpeg_to_rgb` | 効かない（デコーダ任せ） | DirectShow | `MEDIASUBTYPE_MJPG` |
| RGB24 | `push_bgr24` | `bgr24_to_rgb` | 効かない（RGB のまま） | DirectShow | `MEDIASUBTYPE_RGB24` |
| そのほか | `push_decoded` | nokhwa のデコーダ | 効かない（デコーダ任せ） | Media Foundation | — |

- **NV12 / I420 / YV12 は YUY2 と同じ係数表と同じ 1 画素の式を通る。** 同じ Y・Cb・Cr なら YUY2 と同じ RGB になる（`yuv420.rs` のテストでカラーバーを突き合わせている）。違うのは色差の置き場所だけで、4:2:0 なので縦横 2x2 画素が 1 組の色差を共有する。統計でも高速パスとして数える
- 幅か高さが奇数なら、色差は切り上げた大きさを持つものとして読む（右端の列・下端の行は 1 画素で 1 組）。YUY2 の奇数幅は最後の 1 画素を黒で残すが、4:2:0 は全画素を変換する
- 各面の行に詰め物（ストライドの余り）は無いものとして読む。足りないフレームは捨てる。**DirectShow の非圧縮 YUV では `biWidth` が行の長さ（ストライド）を表し、有効な範囲が狭いときは `rcSource` / `rcTarget` で示す決まり**なので、`biWidth` を幅として読めば Y 面の行の余りは出ない。YUY2 の経路も同じ前提。`rcTarget` で切り出す、あるいは `biWidth` と食い違うストライドのサンプルを検出する仕組みは入れていない（実機で必要になったら足す）
- DirectShow で形式が未指定のときは、この表の上から順（YUY2・NV12・I420・YV12・MJPEG・RGB24）に選ぶ。係数表の効く形式を先にしてある（`SampleKind` の並び）
- YV12 は I420 の U 面と V 面が逆に並んだもの。GUID も別なので、I420 には混ぜずに別の形式として扱う
- **`FrameBuffer` へ積む `VideoFrame` は、画素データの長さが必ず `幅 × 高さ × 3` ちょうど。** UI スレッドの `egui::ColorImage::from_rgb` は長さが違うと assert で落ちる（release は `panic = "abort"`）。自前の変換はこの長さで書くが、`push_decoded` が受ける nokhwa のデコーダの出力は入力の長さから決まる（幅が奇数の YUYV、行に詰め物のあるバッファで合わなくなる）ので、入口で `frame_len_status` を見て長ければ切り詰め、短ければ捨てる（#309）。`update_video_texture` も合わないフレームは描かずに前のテクスチャを保つ（二重の守り）

`FrameBuffer` はフレームを `Arc<VideoFrame>` で保持し、取り出し側へは `Arc` の複製を渡す。**画素データを複製しないので、取り出しても 1080p で 6MB の memcpy は発生しない。**

### 毎フレームの大きな確保を避ける（#322）

画素データの複製をしないことに加えて、**1080p60 で毎フレーム数 MB の確保と解放を繰り返さない。** 大きな確保は Windows ではヒープを経ずにページ単位で取られ、触るたびにページフォールトとゼロ埋めが起きる（フェイクの 1080p60 で毎秒 6 万回を超えていた）。

- **フレームコールバック（`FrameSink`）**: `FrameBuffer` が置き換えたフレームを 1 世代遅らせて回収し、次の変換先にする。他に持ち主がいなければ（`Arc::get_mut` が取れれば）**`Arc` ごと**使い回すので、画素の Vec も `Arc` の箱も確保しない（`fill_recycled`）。UI スレッドがテクスチャ化のために握っている、または録画のリング（`VideoTap`）が持っているフレームは書き換えず、新しく作る。この取りこぼしは録画中だけ `VideoTap` が数える
- **フレームバッファのロックの中では解放も起こさない。** どの受け口も回収待ちをロックの前に取り出す（`fill_frame`）。置き換えたフレームはロックの中で回収待ちへ移すだけで、手放すのは次のフレームでロックの外。デコーダの経路（`push_decoded`）はデコーダが確保した Vec を受け取るので画素の確保は避けられないが、`Arc` は使い回し、古い Vec の解放はロックの外で起きる。MJPEG の展開に失敗したときも展開先を捨てずに次へ回す
- **UI スレッド（`update_video_texture`）**: `egui::ColorImage` を `Arc` で手元にも残し、egui が描画を終えて手放していれば、次のフレームはその `Vec<Color32>`（1080p で約 8MB）へ詰め直して渡す（`reuse_or_new_color_image`）。egui はテクスチャの更新を描画の終わりにレンダラーへ渡し、渡し終えたら `Arc` を手放すので、次の `update()` では普通は使い回せる。RGB → `Color32` の全画素の変換そのものは残る（コールバックが RGBA で書けば消せるが、録画の `rgb_to_nv12` とスクリーンショットにも波及するので採っていない）
- **録画のエンコーダ MFT（`recording/encoder.rs`）**: 同期型は 1 枚ごとに `ProcessOutput` を「出る → 入力が足りない」まで呼ぶ。自分で渡す出力のサンプルを 1 つ持って使い回し、毎回の `MFCreateAlignedMemoryBuffer` をやめた。中身は `EncodedSample` へ写し取ってから次に回し、使う前に属性（`MFSampleExtension_CleanPoint` など）と時刻・長さを消す（`writer.rs` の `reset_for_reuse`）。エンコーダが書くのは出力の数十 KB だけでページフォールトはほとんど変わらず、効くのはカーネル側の確保と解放の分だけ（小さい）
- 録画の入力側（NV12 を毎フレーム `memory_sample` で `MFCreateMemoryBuffer` に写す）は、まだ使い回していない

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

## 映像の上に重ねる表示の層

映像の上に常設で出す表示（統計 OSD・フェイクデバイスの帯・録画中の印）は、**`egui::Area` ではなく映像と同じ背景の層（`LayerId::background()`）へ、映像のあとから描く**（`src/app/video_overlay.rs` の `show_video_overlay`、#284）。

- `Order::Foreground` の Area だと、設定ダイアログ（`egui::Window` は `Order::Middle`）の上に重なってダイアログの文字が読めなかった
- `Order::Middle` にしても直らない。egui は「前のフレームで見えていなかった Area」を同じ層の一番上へ移すので、ダイアログを開いたまま情報表示をオンにしたり録画を始めたりすると、やはりダイアログの上に来る
- 背景の層はどの Window よりも下に描かれ、同じ層の中では後に描いたものが上になる。**呼ぶのは映像の `CentralPanel` を描いたあと**
- 「ダイアログを開いている間は隠す」は採らない。ダイアログを開いたまま FPS を見たいことがある
- Area のように前のフレームの大きさを覚えられないので、中身を 1 回目は描かずに測り、2 回目で寄せた位置へ描く
- **トースト（`overlay.rs` の `TransientOverlay`）と右クリックメニューは `Order::Foreground` のまま。** トーストはダイアログの中の操作（プリセットの切り替えなど）の結果も知らせるので、下に隠れると役に立たない。出るのも数秒だけ

## UI にあるが動作していない設定がある

以下は設定画面から変更できるが実装が追いついていない。README の記述もこれらを前提に書かれているため、修正時は README も合わせて更新すること。

- ビデオフォーマットの MJPEG / RGB24（Media Foundation の経路では内部で YUYV に強制される。DirectShow の経路〈「(DirectShow)」のデバイス〉では選んだ形式で開く）

オーディオのサンプリングレート／チャンネル数は `select_best_config` でストリームに反映され、**選択肢も入出力デバイスの対応設定から生成している**（`audio::selectable_sample_rates` / `selectable_channels`）。デバイスの能力を取得できなかった場合だけ固定の既定一覧へ倒すので、そのときは対応しない値も選べる。
