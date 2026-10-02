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

## DirectShow で開く解像度（#391）

**入力信号と違う解像度で開くと、映像の代わりに自前の警告画面（「Signal Out of Range」）を送ってくるボードがある**（AVerMedia GC551、DirectShow で開く）。解像度を変換しないボードで、開いた解像度が入力と一致するときだけ映る。警告画面も正しい大きさの YUY2 のフレームとして届くので、**アプリからは見分けられない。** 開く解像度を入力に合わせるしかない。

- **ドライバーが返すいまの形式（`IAMStreamConfig::GetFormat`）を手掛かりにする。** GC551 は入力が 1920x1080 60Hz なら、前に 1280x720 で開いたあとでも 1920x1080 60fps を返した（実機で確かめた）。`SetFormat` すると設定した値が返るようになるので、**読むのは `SetFormat` の前**（`devices::current_resolution`）。入力を切り替えたときに追従するかは、入力側の解像度を変えられる環境が無く確かめていない
- 開くとき（`graph::apply_format`）は、設定の解像度がそのデバイスの対応一覧に無いか未指定なら、いまの解像度で開く（`devices::target_resolution`）。**一覧にある解像度を利用者が選んでいれば上書きしない。** 解像度を変換できる DirectShow のデバイス（Web カメラ、OBS の仮想カメラ）で、選んだ解像度が効かなくなるため。fps の扱いは変えない
- **初回（設定ファイルが無い）は設定の解像度を未指定にして開く。** 既定の 1280x720 はデバイスに合わせて選んだ値ではないため。ワーカーの `resolve_default_devices` が映像デバイスの名前を埋めるときに解像度を外し、UI も `DefaultDevicesResolved` を受けて外す。開けたら実際の解像度を `VideoResolutionResolved` で返し、UI が設定へ書き戻す（次の起動からはその解像度を指定して開く）。書き戻すのは、設定の解像度がまだ未指定で、名前・形式・fps・開き方がイベントの運ぶ接続対象と同じときだけ（`device::should_store_resolved_resolution`。届くまでに利用者が選び直した値を上書きしない）。Media Foundation の経路は未指定なら従来どおり 1280x720 を要求するので、開き方は変わらない
  - 書き戻すまでの間に UI が 2 秒ごとに送る設定（解像度なし）が遅れて届くと、そのまま比べれば差分ありで開き直してしまう。ワーカーは開いた相手と解像度以外が同じなら開いた解像度を引き継ぐ（`worker_commands::carry_resolved_resolution`）
- 設定ダイアログでデバイスを切り替えたときの既定（`ui::video_mode::select_default_video_mode`）も、能力に添えたいまの解像度（`FormatCapability::current_resolution`、DirectShow だけが埋める）を前の解像度より優先する。前のデバイスの 1280x720 を引き継ぐと映らないため
- 録画の「大きさの変化で停止」（`docs/design/recording.md`）とは関係しない。入力が変わってボードが警告画面に切り替わっても、フレームの大きさは開いた解像度のまま

## DirectShow で開く fps（#389）

`IAMStreamConfig::GetStreamCaps` の fps は点ではなく範囲（`MinFrameInterval` 〜 `MaxFrameInterval`）で返る。GC551 は 15 〜 60.0002fps。

- **範囲の中なら要求した fps をそのまま `AvgTimePerFrame` に入れて開く**（`devices::fps_range` / `choose_candidate`）。範囲を持たない（両端が同じ fps に丸まる）デバイスは、これまでどおり一覧の中で最も近い fps
- **設定画面の選択肢は、範囲を持つ形式では `GetFormat` の `AvgTimePerFrame` が示すいまの fps だけにする**（#410、`devices::current_format` / `fps_choices`）。下の実測のとおり、どれを選んでも届くのは入力信号の fps なので、ほかの値を並べても意味が無い（ユーザー指摘 2026-10-02）。GC551 は入力が 1920x1080 60Hz なら 60 を返し、1920x1080 の選択肢は 60 だけになる（`directshow::tests::capabilities_list_only_the_input_fps_for_ranged_formats`）。いまの fps が読めないか範囲の外なら、メディアタイプの既定値と両端に範囲の中の代表値（15 / 24 / 25 / 30 / 50 / 60 / 120）を足した一覧（`devices::fps_list`、#389 のときの並べ方）へ戻す。範囲を持たない形式は一覧のまま
- 選択肢を絞っても開き方は変えない。設定ファイルに選択肢に無い fps が書かれていても、範囲の中ならその値で開く
- **開いた fps と届く fps は一致するとは限らない。** GC551（入力 1920x1080 60Hz）で 30fps を要求すると、`SetFormat` は通り「開いた fps」は 30 になるが、届くフレームの間隔は平均 16.67ms（59.996fps）のままだった（2026-10-01 の実測、`directshow::tests::start_capture_requested_fps_inside_the_range_reports_the_delivered_rate`）。ボードが入力信号の fps で出しているためで、アプリの不具合ではない。間引いて 30fps にする処理は入れていない
- 解像度が未指定のときは fps の指定を見ず 60fps を要求する（`graph::apply_format`）。これは変えていない

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
- **最小化中も起こさない。** eframe 0.36 は最小化中のウィンドウにも `update()`（このアプリの `App::ui`）を回す。最小化したウィンドウには `RedrawRequested` が届かないので、`eframe::native::run` が期限の来たウィンドウを `is_invisible_or_minimized` で拾い、`run_ui_and_paint` を直接呼ぶ。ただし間隔は 100ms（`INVISIBLE_WINDOW_REPAINT_INTERVAL`）より詰めず、他スレッドからの再描画要求も同じ下限へ丸める。画面には何も見えないので、起こしても見えない `update()` が最大で毎秒 10 回増えるだけになる。そのため最小化中は `should_wake_on_event` が偽を返し、`next_repaint_delay` は 1 秒を予約する（`src/repaint.rs`。最小化の判定は `src/app/mod.rs` の `viewport.minimized`）。**`update()` は止まらないが 1 秒ごとにしか来ない**ので、そこで動く処理（OSD の消滅、設定の遅延書き出しなど）は最大 1 秒遅れる。接続の再試行と切断の監視はデバイスワーカースレッドへ移したので影響を受けない（#133）。ホットキーのアクションも、画面が要らないもの（デバイス再接続・音量・ミュート）はリスナースレッドからワーカーへ流して実行する。画面が要るもの（フルスクリーン切替など）だけが復帰まで持ち越される（`docs/design/hotkeys.md` の「最小化中の扱い」）。最小化中に閉じる要求で終了しなかった問題（#420）は再描画とは別の原因で、`docs/design/window.md` に書いてある
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
