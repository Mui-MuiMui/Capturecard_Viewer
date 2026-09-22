# デバイスワーカースレッド

デバイスを開く・閉じる・列挙する処理を専用スレッド 1 本へ隔離した理由と、そこで決めた取り決め。
UI スレッドとの共有の仕方、開き直しの差分判定をどちら側に置くか、最小化中の扱いを扱う。
目指す構造そのものは `docs/ARCHITECTURE.md` の「並行性」にある。

## デバイス操作はワーカースレッドが行う

**デバイスを開く・閉じる・列挙する・能力を問い合わせる処理は、すべて専用のワーカースレッド 1 本の上で起きる。** UI スレッドは `DeviceCommand` を送り、`DeviceEvent` を `update()` の中で非ブロックに受け取るだけ。`start_capture` は実測 20〜93ms、`start_passthrough` は 300ms 前後、音声デバイスの列挙も 300ms 前後かかる。これを `update()` の中でやっていた頃は、接続を試すたびに描画がその分だけ飛んでいた。

```mermaid
flowchart LR
    ui["UI スレッド<br/>CaptureCardViewer"]
    worker["device-worker スレッド<br/>WorkerState"]
    dev["VideoCapture / AudioCapture"]

    ui -->|DeviceCommand| worker
    worker -->|DeviceEvent| ui
    worker -->|Arc&lt;RwLock&gt; DeviceSnapshot| ui
    worker --> dev
    dev -.->|VideoFrames / Atomic| ui
```

**チャネルを通さない共有が 4 つある。** どれもデバイスを開く処理を挟まないので、コマンドの列に並べる理由がない。

| 共有するもの | 型 | 触る側 |
|---|---|---|
| 映像フレーム | `video::VideoFrames`（`Arc<Mutex<FrameBuffer>>`） | フレームコールバックが書き、UI スレッドが読む |
| 色空間・レンジ・明るさ・コントラスト・彩度 | `Arc<video::SharedColorConversion>`（Atomic） | UI スレッドが書き、フレームコールバックが読む |
| 音量・ミュート・パススルー | `Arc<audio::AudioControls>`（Atomic） | UI スレッドが書き、出力コールバックが読む |
| 音声のリサンプル補正の水位・補正係数 | `Arc<audio::ResampleTelemetry>`（Atomic） | 出力コールバックが水位を書き、デバイスワーカーが `tick` の中で補正係数を書く。**入出力の形が揃っている（identity）ストリームでは作らない**（`AudioCapture::resample_telemetry()` が `None` を返す） |

**映像フレームを `DeviceEvent` で送らないこと。** 接続やデバイス列挙の後ろで待たされ、遅延が増える。

これとは別に、「いま何に繋がっているか」もチャネルを通さず `DeviceSnapshot`（`Arc<RwLock<..>>`）で共有する。UI は `update()` の先頭で 1 回だけ複製を読み、描画も「接続状態」タブもそこから引く。**イベントを取りこぼしても表示が食い違わないよう、状態は必ずこちらを正とする。** ワーカーはイベントを送る前に観測値を書き出すので、UI 側は**イベントを取り込んでからスナップショットを読む**。逆にすると「接続に失敗した」を受け取ったフレームで失敗前の観測値を見る。

**開き直しが要るかの差分判定はワーカー側が持つ。** UI は 2 秒ごとに `DeviceConfig` を丸ごと送り、ワーカーが前回接続できた対象（`last_video_target` / `last_audio_target`）と比べる。UI 側に記録を置くと、接続の成否を知っているのはワーカーなのに記録は UI、という分かれ方になる。

- コマンドは受けた順に 1 つずつ処理する。**ワーカーは止まってよい。** 音声の対応設定が無ければ開く直前にその場で取りに行く（以前は届くまで接続を見送る仕組みを UI 側に持っていた）
- ループはコマンドを待つ前に期限（`tick`）を片付ける。こうしないと、起動直後に積まれる「デバイス能力の取得」（数百 ms）の後ろで最初の接続が待たされる
- コマンドを待つ間隔は、接続を追いかけている間が 100ms、それ以外が 500ms（`next_tick_delay`）。バックオフの最小値が 200ms なので、それより細かく起きる意味はない
- 終了は `on_exit` が `DeviceWorker::shutdown()` を呼んでストリームを閉じさせ、join する。**スクリーンショットの保存スレッドと同じ扱い。** 待たないと閉じる途中でプロセスが落ちる
- **`AudioCapture` はワーカースレッドの中で作る。** `cpal::Stream` は `!Send` で、作ったスレッド以外へ持ち出せない

## 最小化中も再接続と切断監視は止まらない

接続の再試行・フレーム途絶の検出・音声ストリームのエラーの回収・Windows 側の既定デバイスの追従は、どれもワーカー自身のタイマーで回す。**`update()` からは何も駆動しない。**

eframe は最小化されたウィンドウの再描画要求を捨てるため、最小化中は `update()` が呼ばれない。`update()` から駆動していた頃は、その間これらがすべて止まっていた（#133）。**時間で動くデバイス処理を `update()` から呼ぶ形へ戻さないこと。**

ホットキーのアクション実行も、**画面が要らないもの（デバイス再接続・音量・ミュート）はリスナースレッドからこのワーカーへコマンドとして流す**（`DeviceCommand::ReconnectNow` / `AdjustVolume` / `ToggleMute`）。ワーカーがウィンドウの状態に関係なく動く唯一のスレッドだからで、UI スレッドの代役をここに置いている。実行したことは `DeviceEvent` で返し、復帰した最初のフレームで UI 側の設定と OSD を追従させる。画面が要るもの（フルスクリーン切替、最前面表示、スクリーンショット）は復帰まで持ち越す。詳しくは `docs/design/hotkeys.md` の「最小化中の扱い」。

## デバイスに触る入口は trait 1 枚で仕切る

**ワーカーは `app::backend` の `VideoBackend` / `AudioBackend` 越しにしかデバイスへ触らない。** `worker_loop` / `worker_connect` / `worker_timers` はどれも `Box<dyn ..>` を持つだけで、`VideoCapture` / `AudioCapture` という具体型を知らない。実装を選ぶのは `DeviceWorker::spawn` の 1 か所（`SystemBackends`）で、そこが `BackendShared`（フレーム・色変換・音量・再描画の窓口）と一緒にワーカースレッドへ送り、**組み立てはあちら側で行う**（`cpal::Stream` が `!Send` なので、作る場所は使うスレッドでなければならない）。

```mermaid
flowchart LR
    spawn["DeviceWorker::spawn<br/>（UI スレッド）"]
    loop["worker_loop / worker_connect<br/>worker_timers"]
    trait["VideoBackend / AudioBackend"]
    real["VideoCapture / AudioCapture"]
    mock["モック（テスト専用）"]

    spawn -->|Box&lt;dyn DeviceBackends&gt;| loop
    loop --> trait
    trait --> real
    trait -.-> mock
```

**境界はワーカーがデバイスへ触る場所に置く。** 開く・閉じる・列挙する・能力を問い合わせる・観測値を読む、の 5 つだけで、`worker_connect` と `worker_timers` が呼ぶ操作がそのまま trait のメソッドに並ぶ。ここより上（コマンドの解釈、再試行の期限、途絶の判定）はもともと `WorkerState` と `monitor` / `retry` の側にあり、デバイスを知らない。ここより下（`src/video/` / `audio.rs` の中身）には手を入れていない。

**開いた結果を別のハンドル型では返さない。** ストリームを持つのは実装自身で、`stop_capture` / `link_state` / `active` がその持ち物に対する窓口になる。`VideoCapture` は `CallbackCamera` を抱えたまま開き直しと途絶の観測を行っているので、「開いた分」だけを切り出すには `src/video/capture.rs` の中身を動かすことになる。trait を被せる目的はそこではない。

**フレームコールバックと cpal のコールバックの経路には挟まない。** 映像フレームは `VideoFrames`、音量とミュートは `AudioControls` の共有ハンドル越しに今までどおり流れる。あの 2 つのコールバックはロックもアロケーションもしない決まりで（`docs/design/video-pipeline.md` / `docs/design/audio.md`）、動的ディスパッチを足す場所ではない。trait 化したのは開閉と問い合わせだけなので、1 回の接続につき数回しか通らない。

### これで実機なしに何が試せるか

テスト用のモックは `app::backend` の `mock`（`#[cfg(test)]`）にある。持たせたのは「指定回数失敗してから成功する」「列挙結果を差し替える」「フレームが止まったことにする」「音声ストリームのエラーを起こす」の 4 つだけで、映像や音声の中身は作らない。

モックを載せた `WorkerState` は、スレッドを起こさずに `tick(now)` を呼べる。**渡す時刻はテストが決めてよい**ので、バックオフ（200ms → 400ms → …）もフレームの途絶（3 秒）も実時間を待たずに跨げる。`ConnectRetry` と `monitor` がもともと `Instant` / `Duration` を引数で受け取る形だったため、時刻の注入のために足した仕組みは無い。

**カラーバーや正弦波を吐くフェイクデバイスはここには無い。** それはこの trait の実装の 1 つとして #142 で足す。色変換の期待値の検証や、デバイス切替 UI・自動復帰・スクリーンショットの通し確認はそちらの話で、ここにあるモックは「ワーカーの分岐を通す」ためだけのもの。
