# Run and Log 開発メモ

Markdown 内のシェルコマンドを実行し、結果を同じ Markdown に書き戻すツール。
仕様と使い方は README.md を参照。ここには開発上の判断と注意点だけを書く。

## 言語の方針

public リポジトリなので、**README・コードコメント・UI 文字列・エラーメッセージはすべて英語**で書く
(CYBERNEURA-DEV-442 での指示)。**この CLAUDE.md だけは日本語**。エージェント向けの開発メモで、
他リポジトリでも「ユーザー向け表示は英語、開発者向けドキュメントは日本語」で揃えているため。

例外は `parse::is_designation_head` が受け付ける `結果:`。これは UI 出力ではなく
**入力として許容する綴り**で、日本語で書かれた Markdown を読めるようにするために残している。

## 設計の要点

- **コアは IO を持たない**。`runandlog-core` はパース (`parse`)・実行 (`exec`)・整形 (`render`)
  だけを担当し、ファイルの読み書きは `runandlog` クレート (`crates/runandlog-cli/`) の `session` が行う。TUI・非対話実行・
  将来の GUI が同じコアを共有できるようにするため。
- **書き戻しは範囲置換だけで行う**。`parse` は結果をバイトオフセットで保持し、`splice` が
  該当範囲を置き換える。Markdown を再構築しないので、原文の書式がそのまま残る。
- **結果は HTML コメントのマーカーで挟む**。再実行時に前回の結果を確実に特定して
  置き換えるため。マーカーが無いと、結果が追記され続けるか、本文を巻き込んで壊す。
- **stdout と stderr は 1 本のパイプで受ける** (`std::io::pipe`)。別々に読んで連結すると
  出力の順序が壊れ、ログとして読めなくなる。

## 実装上の注意

- `exec::run` はコマンドを専用のプロセスグループで起動し、タイムアウト時はグループごと kill する。
  シェルだけを kill すると、シェルが起動したコマンドが走り続けたうえパイプも開いたままになり、
  出力の読み取りが終わらない。タイムアウト無しの場合はこの保護が効かないので、読み取りは
  `DRAIN_GRACE` で打ち切って読めた分を結果とする。
- **spawn 後に `Command` を drop してパイプの書き込み端を手放す。** `Command` は
  `stdout` / `stderr` に渡した書き込み端を保持し続けるので、drop しないと親側にコピーが残り、
  子が終了しても読み取りスレッドに EOF が来ない。結果、**全実行が毎回 `DRAIN_GRACE` (300ms) を
  待たされ、報告する実行時間もその分水増しされる**。回帰を防ぐテストが
  `exec::tests::a_fast_command_does_not_wait_for_the_drain_grace`。
- `ExecOutcome::duration` はプロセス終了時点で確定させる。drain の待ち時間はコマンドの
  実行時間ではないので含めない。
- 出力の取り込みには上限がある (`ExecOptions::max_output_bytes`、既定 8 MiB)。上限を超えても
  読み捨てるだけで読み取り自体は続ける。読むのをやめるとパイプが詰まってコマンドが止まる。
- **`out=` や `Result:` で指定された書き出し先は信用しない。** Markdown の中身は AI エージェントが
  書くこともある untrusted input なので、絶対パスや `..` を含む指定は `render::is_inside_dir` で
  弾き、Markdown のあるディレクトリの外へ書けないようにしている。
- 結果ブロックの終了マーカーを探すときはコードフェンスを読み飛ばす (`parse::find_end_marker`)。
  コマンドの出力自体が終了マーカーと同じ行を含むと、結果ブロックの範囲を取り違えるため。
- Markdown への書き込みは一時ファイル経由の rename (`session::write_atomically`)。
  実行中に中断されても元ファイルが半端な状態にならないようにする。書き込み直前に
  `refresh_before_write` でファイルを読み直し、実行中の外部編集を握り潰さないようにしている。
- TUI の実行はワーカースレッドに投げる。同期実行にすると長いコマンドで画面が固まる。
- **Ctrl-C はアプリが拾って明示的にプロセスグループを kill する** (`Canceller`)。コマンドを
  専用のプロセスグループで起動している以上、端末が前面プロセスグループに送る SIGINT は
  runandlog にしか届かない。ハンドラを入れないと、runandlog だけが死んでコマンドは生き残り、
  ユーザーが待っていた出力ごと消える。
  - 非対話実行 (`main::catch_interrupts`) は SIGINT / SIGTERM をハンドラで受けて**受信した
    シグナル番号**を記録し、監視スレッドが `Canceller::cancel` を呼ぶ。**ハンドラの中では kill しない**。
    `cancel` はロックを取るのでハンドラから呼べず、そもそもハンドラはシグナル番号しか受け取れないので
    `Canceller` に辿り着くには static に置き直す必要がある。
    終了コードは `128 + signo` (SIGINT なら 130、SIGTERM なら 143)。2 回目のシグナルは
    書き戻しを待たずに `_exit`。
  - **セルの前後で見るのはハンドラが直接書く `INTERRUPTED_BY` で、`Canceller` ではない。**
    監視スレッドは 50ms ごとにしか起きないので、シグナル受信直後は canceller がまだ何も知らない。
    canceller を見ると、シグナル受信済みなのに次のセルを開始してしまい、下に書いた上書きが起きる。
    最後のセルの直後にシグナルが来た場合に終了コードが 130 / 143 にならない問題も同じ原因。
  - **キャンセルのフラグはセルを実行する前にも見る。** 前のセルの書き戻し中に Ctrl-C が届くと、
    次のセルを開始した直後にキャンセルされ、**出力ゼロの結果がそのセルの前回結果を潰す**。
    誰も頼んでいない実行で結果を失うことになる。GUI の `run_all` も同じ理由で
    `stop_requested` をセルの前に見る。
  - TUI は raw mode で SIGINT 自体が来ないので、Ctrl-C をキーイベントとして受けて cancel する。
    `q` / `Esc` は従来どおり「終わってから終了」。
  - `Canceller::cancel` はフラグ→pid の順、`run` は pid→フラグの順に触る。spawn の最中に
    cancel が来ても、どちらかが必ず相手を見るので取りこぼさない。
  - **pid は atomic ではなく `Mutex<i32>`。** kill する側は「id を読む→kill する」の間、
    reap する側は「reap する→id を捨てる」の間、互いを排他する必要がある。
    読んでから kill するまでスレッドは任意の長さ止まりうるので、その間に子が reap されて
    pid が再利用されると、**無関係なプロセスグループを kill する**。ロックを取るのは
    20ms ごとのポーリング 1 回ぶんで、キャンセル要求のポーリング自体は AtomicBool のまま。
    この代償として `cancel` はシグナルハンドラから呼べない (呼んでいない)。
  - **シェルが終了してから drain (最大 300ms) が終わるまでの Stop は何も kill しない。**
    リーダーを reap した後もプロセスグループは子孫が残っていれば存続するが、その時点では
    「グループが消えて番号が再利用された」のか「元の子孫がまだ属している」のかを区別できない。
    パイプを掴んだ子孫は生き残る。
  - **キャンセルされた実行も結果を書き戻す。** 途中まで出た出力を残すのがキャンセルの目的。

## 既知の制約: unix 以外は未対応

`exec` のプロセス管理は **unix でしか成立していない**。Windows では
`kill_process_group` がプロセスグループを持てずシェルしか kill できないため
**タイムアウトが効かず**、生き残った子孫がパイプを掴んだままになる。さらに
`make_reads_interruptible` も no-op なので、その状態になると読み取りスレッドと
パイプハンドルが子孫の寿命ぶんリークする (CYBERNEURA-DEV-442 の Codex レビューで
指摘された)。

直すには Windows 側で job object と overlapped read が要るが、**この開発環境では
ビルドも検証もできない**ため手を付けていない。Windows を対象にするなら、まず
その環境で検証できる CI を用意すること。

## GUI (Tauri)

`--gui` で Tauri のウインドウを開く (CYBERNEURA-DEV-445 で実装)。

- **`runandlog-core` と `session` をそのまま使う。** パース・実行・書き戻しの経路は
  TUI・非対話実行と完全に同じで、GUI 側には Markdown の仕様が一切無い。
  ここを分岐させると仕様の追従が破綻するので、GUI 専用のパースを足さないこと。
- **`gui` は cargo feature** (既定 ON)。Tauri はビルドに webkit2gtk / gtk3 を要求するため、
  CLI / TUI だけが欲しい配布者は `--no-default-features` で外せる。外したビルドでも
  `--gui` は受け付けて「この build に GUI は無い」と答える (clap の未知フラグにしない)。
- **実行はブロッキングワーカーに投げる** (`tauri::async_runtime::spawn_blocking`)。
  `runandlog_core::run` は完了までブロックするので、コマンドスレッドで直接呼ぶと
  ウインドウがコマンドの実行時間ぶん固まる。TUI がワーカースレッドを使うのと同じ理由。
- **`Session` のロックを `await` をまたいで持たない。** またぐと `MutexGuard` が Send でなく
  コンパイルが通らないうえ、通ったとしても実行中ずっと画面の読み取りを止めてしまう。
- **同時実行は 1 本に制限する** (`busy` フラグ)。同じファイルを 2 本が書き戻すと、
  互いの書き込みを「外部編集」と見なして `refresh_before_write` が両方失敗する。
- **reload も同じ `busy` フラグを取る** (Codex レビューで Critical として指摘)。
  `apply_outcome` は「実行前に持っていた文書」と実際のファイルを比べて外部編集を検出するが、
  実行中に reload するとその基準がディスクの現在値に差し替わり、**比較が通ってしまう**。
  ガードが守ろうとしている当のものでガードが無効化され、古いコマンドの結果が
  別のセルに書き戻されうる。回帰テストは `reloading_is_refused_while_a_command_is_running`。
- **Stop は実行ごとに新しい `Canceller` を持たせる** (`GuiState::arm` / `stop`)。使い回すと
  一度 Stop したフラグが立ちっぱなしになり、次のコマンドが即キャンセルされる。実行が
  終わったら `arm(None)` で外す。遅れて届いた Stop が次のコマンドに当たらないようにするため。
  `run_all` はキャンセルされたセルで打ち切る (`RunReport::cancelled`)。
- **GUI の操作状態 (`busy` / `stop_requested` / `Canceller`) は 1 つの `Mutex<Operation>` で扱う。**
  別々の atomic にすると「走っているか」「Stop を覚える」「コマンドを kill する」の間に
  操作の終了と次の操作の開始が割り込め、**前の操作の Stop が次の操作の 1 セル目に当たる**。
- **`Canceller` を外している間の Stop は `stop_requested` フラグで拾う。** `Canceller` は
  コマンドが走っている間しか存在しないが、フロントは `runandlog://started` で Stop を有効にした後、
  操作が終わるまで有効なままにする (セル間でも押せる)。`run_all` のセル間 (書き戻しの実 IO を挟む
  ms オーダーの窓) に押された Stop は、フラグが無いとどこにも残らず、**「止めた」と表示しながら
  バッチが走り続ける**。
  フラグは `acquire` (操作の開始) でクリアする。
  Stop でバッチが終わったことは `BatchReport::stopped` で返す。キャンセルされた実行が 1 つも
  無いまま終わる場合があるため、`RunReport` からは読み取れない。
- **フロントは `listen` の購読が終わるまでボタンを有効にしない。** Stop は
  `runandlog://started` を受けて初めて有効になるので、購読前に実行を始められると
  **コマンドが走っている間ずっと Stop が押せない**。購読は all-or-nothing で扱う
  (`Promise.allSettled` → 一部でも失敗したら成功したぶんを unlisten する)。半分だけ
  購読された状態を作らないため。
- **購読に失敗しても縮退して開く。** capability 不足などで `listen` が拒否されたら、
  ドキュメントを読んでボタンを有効にし、理由をステータスに出す。ここで固まらせると
  ウインドウが何もできない箱になる。この状態では `started` が来ないので、**Stop は
  busy に合わせて有効にする** (既定はタイムアウト無しなので、押せないと長いコマンドを
  ウインドウから止められない)。早すぎる Stop はバックエンドが「何も走っていない」と
  返すだけで無害。
- **フロントは innerHTML を使わない。** コマンドとその出力は任意のテキストで、
  HTML として流し込むとドキュメントがウインドウ内でスクリプトを実行できてしまう。
  `textContent` と DOM 生成だけで組む。
- **capability を置かないとコアプラグインのコマンドが拒否される** (`capabilities/default.json`)。
  `invoke_handler` に登録した自前コマンドは capability 無しでも通るので、**`event.listen` だけが
  静かに失敗する**。実機で開くまで気付かなかった。与えるのは `core:event:default` だけで、
  window / webview / menu / path は使っていない。
- ウインドウは `tauri.conf.json`、フロントの実体は `crates/runandlog-cli/ui/`
  (素の HTML / CSS / JS。node のビルド工程を持たないので `withGlobalTauri` を有効にしている)。

**ヘッドレスなのでウインドウの目視確認はできない。** ビルドと `cargo test` は通るので、
検証はそこまで。実機で開く確認は動作確認できる環境で行うこと。

## アプリアイコン

`crates/runandlog-cli/icons/icon.png` (`tauri.conf.json` の `bundle.icon`)。
**手で描かずに `scripts/generate-icon.py` で生成する** (標準ライブラリだけで動く。
このリポジトリに画像ツールチェーンは無く、アセット 1 枚のために足す価値も無いため)。
変更したらスクリプトを直して再生成すること。

広く使われている macOS のアイコンテンプレート (Big Sur 以降) の寸法を、スクリプトの
定数にしてある (CYBERNEURA-DEV-519。**以前のアイコンは 64x64 の角丸なし・余白なしの
単色正方形**で、Dock で他のアイコンより大きく・四角く見えていた):

- canvas 1024x1024 に対し、**本体は 824x824 を中央に置く**。四辺に 100px (約 10%) の
  透明な余白が残る。テンプレートに従ったアイコン同士が同じ大きさに見えるのはこの余白の
  ぶんで、**canvas いっぱいに描くと隣のアイコンより大きく描かれる** — これが
  「四角くて変」の正体。影やバッジが乗る場所でもある。
- **角丸は本体の 22.5%** (185.4px)。Apple の実物は円弧ではなく連続曲率の squircle だが、
  この半径の円弧近似がテンプレートのグリッドで、実際に見えるサイズでは見分けが付かない。
- 上記 3 つ (canvas / 本体 / 角丸) 以外 — グラデーション・ハイライト・再生マーク — は
  このアプリの意匠なので自由に変えてよい。

生成後の確認は、**アルファの範囲を数えて寸法を検算する**のが確実
(本体の範囲が 100..923 の 824x824 になっているか)。ヘッドレスなので Dock での
見え方は確認できないが、縮小して暗い背景に合成すれば小サイズの潰れは見られる。

## リリース

**`Cargo.toml` の version を main で変えると、それがリリースになる。** ワークフローは
**main への push すべて**と `workflow_dispatch` で起動し、**その version が既にリリース済みか
どうか**だけで実行するか決める。`scripts/release.sh [patch|minor|major]` は
version を書き換えて push するだけの薄いスクリプトで、手で Cargo.toml を編集して push しても同じ。

- **paths フィルタは付けない。** `Cargo.toml` に絞ると安く済むが、ビルドや workflow や
  コードの不具合で失敗したリリースを「原因を直して push」で再試行できなくなる (その修正は
  Cargo.toml を触らない)。判定は checkout と API 1 回で済むので、絞る価値より安い。
- **判定は「diff で version が変わったか」ではなく「その version が公開済みか」。** squash /
  rebase / 直 push で diff の形は変わるが、公開済みかどうかは変わらない。この判定にすると
  **何度実行しても安全**になり、失敗したリリースは原因を直して push すればそのまま続きになる
  (version を上げ直す必要が無い)。
- **公開済み判定は 404 だけを「未公開」と読む。** rate limit や障害を未公開と読むと、公開済みの
  version をもう一度ビルドして publish しにいく。
- **mac は arm64 のみ、既定 feature (GUI 込み) で Developer ID 署名 + 公証。** cask は
  quarantine を付けるので、署名も公証も無いと受け取った側の初回起動が止まる。素のバイナリは
  staple できないのでチケットは Apple 側に残る (Gatekeeper がオンラインで引く)。**公証の結果は
  `--output-format json` の `status` で見る** — 提出が完了しさえすれば `notarytool` は成功
  終了しうる。
- **Linux は `--no-default-features`。** 既定 feature には Tauri が入り、webkit2gtk に動的
  リンクしたバイナリは同じ webkit を持たない環境で起動しない。ただし glibc には動的リンク
  したままなので「どの Linux でも動く」わけではない。
- **Release は draft で作ってアセット数を数えてから公開する。** `gh release create` は先に
  Release を作ってからアップロードするので、途中で失敗すると公開済みで中身の欠けた Release が
  残る。**残った draft は次の実行が消して作り直す** (公開済みには触らない)。draft は
  `/releases` の列挙で探す — **`releases/tags/{tag}` は draft を返さない** (draft に tag は
  まだ無い) ので、tag で引くと同じ名前の draft が 2 つできる。
- **Homebrew cask はこのリポジトリでは触らない。** `cyberneura/homebrew-tap` 側の workflow が
  毎時、各プロジェクトの最新 Release を見て cask を更新する。各プロジェクトから tap へ push する
  形にすると、**tap に書ける token を全プロジェクトに配る**ことになるため。
- **crates.io はリポジトリ変数で有効化する** (`gh variable set PUBLISH_CRATES --body true`)。
  secrets は `if:` で参照できないので変数で分岐する。`CARGO_REGISTRY_TOKEN` が要る。
- **`runandlog` の publish は `--no-verify`。** 検証はパッケージを展開してビルドすることだが、
  その最中に `tauri-build` が `gen/schemas/` をソースディレクトリに書くため、cargo が
  「build script が OUT_DIR の外を書き換えた」として拒否する (v0.3.0 の publish が実際にこれで
  落ちた)。検証を戻す先が無いので、**同じ run の build ジョブ**が同じ commit・同じ feature・
  同じ OS でビルドしていることをもって代える。`runandlog-core` は従来どおり検証する。
- **crates.io は sparse index (`index.crates.io`) に聞く。** cargo が解決に使うのは index で、
  web API に出ていても index が追い付いていなければ次の crate のビルドが落ちる。公開済みの
  version は飛ばすので、片方だけ通った後の再実行が続きから進む。

## 実行中の出力のライブ表示

`exec::run_streaming` (CYBERNEURA-DEV-567)。`run_cancellable` に「出力が届くたびに呼ばれる
コールバック」を足しただけのもので、`run` / `run_cancellable` はこれに委譲する。

- **コールバックは専用スレッド (`report_output`) で呼ぶ。** リーダースレッドから呼ぶと、
  描画が遅れたぶんパイプの読み取りが止まり **パイプが詰まってコマンドが止まる**。かといって
  ポーリングループから呼ぶと、**タイムアウトとキャンセルの検出がコールバックの後ろに並ぶ**
  (誰も読んでいないパイプへ print するフロントエンドで実際に詰まる。Codex レビューの指摘)。
  どちらでもない場所に置いて、遅いフロントエンドが失うのは表示の鮮度だけにしてある。
  回帰テストは `a_front_end_that_blocks_does_not_hold_up_the_timeout`。
  - 刻みは `POLL_INTERVAL` (20ms)。終了は done チャンネルの sender を drop して即座に起こす
    (ポーリング待ちを実行ごとに払わないため)。
  - **最後の 1 回だけは join 後に呼び出し側のスレッドで流す** (公開ドキュメントにも明記。
    同時に 2 スレッドから呼ばれることは無いが、**常に同じスレッドとは限らない**ので、
    コールバックの状態はクロージャに持たせる — thread-local に置かない)。 残りの出力と最終 `output` は同じロックで
    1 つのスナップショットから作る (割り込まれると両者が食い違う) が、**コールバックはロックを
    離してから呼ぶ**。DRAIN_GRACE 超過時はリーダースレッドがまだ動いており、ロックを持ったまま
    遅いコールバックを呼ぶとリーダーが mutex で止まる = パイプが drain されなくなる。
  - コールバックは thread に渡すので `+ Send` が要る。パニックは `resume_unwind` で
    呼び出し側に返す (見えないスレッドで握り潰さない)。
- **チャンクを連結すると `ExecOutcome::output` と完全に一致する。** フロントは「途中経過を
  出す → 最後に結果を出す」を何も突き合わせずに書ける。この不変条件のために:
  - `decode_ready` は**読み取り境界で切れた文字を保留する** (`Utf8Error::error_len() == None`)。
    その場で U+FFFD にすると、最終出力 (`from_utf8_lossy`) には無い置換文字が途中経過にだけ現れる。
  - 逆に**絶対に文字になれないバイトはその場で置換する**。続きを待つと表示が実行中ずっと止まる。
  - ループの最後に、未報告のバイト (保留中の断片を含む) を lossy で 1 回流す。
- 各フロントの使い方:
  - **非対話実行** (`main::print_as_it_arrives`) は届いたそばから書いて**毎回明示的に
    flush** する。stdout は端末なら行バッファ、リダイレクトならブロックバッファなので、
    改行の無い進捗行はライブ表示の本命なのにバッファに残る。
    **`print!` は使わない。** stdout が閉じている (`| head` 等) と panic するマクロで、
    このコールバックは実行の内側から呼ばれるため、**結果を Markdown に書き戻す前に**
    panic が飛び出してコマンドを走らせ損になる (Codex の PR レビュー指摘)。
    `write_all` で書き、失敗したらその実行の残りは黙って捨てる
    (テスト: `printing_gives_up_quietly_once_the_output_has_nowhere_to_go`)。
  - **TUI** はワーカーから mpsc でチャンクを送り、描画スレッドが `try_recv` で拾う
    (`collect_output`)。ロック共有にするとワーカーが描画を待ちうる。表示は
    `live::LiveOutput` の末尾 3 行で、**行数は常に固定**。出力のたびに高さが変わると
    下のセルが動いて読めない。
  - **GUI** は `runandlog://output` イベント (`{index, text}`)。フロントは再描画せず
    `pre.live` の `textContent` を直接更新する (毎秒何度も全セルを作り直さないため)。
    `index` を必ず見て、**走っているセル以外のチャンクは捨てる** (直前の実行の最後の
    チャンクが次のセルの下に出るのを防ぐ)。`runandlog://document` を受けた時点で
    ライブ表示を捨てる — 結果が書き戻された後なので、残すと新しい結果が隠れる。
- **ライブ表示は記録ではないので、どの層でも上限を持つ**: `LiveOutput` は 16KiB、
  GUI の 1 チャンクは 64KiB、フロントの保持は 20000 文字。全量は実行終了時に Markdown へ
  書き戻されるので、捨てても失われない。**末尾を切る時は文字を割らない**
  (Rust は `live::tail` が char boundary、JS は下位サロゲートを 1 つ進める)。
- **`max_output_bytes` (既定 8MiB) に達するとライブ表示も止まる。** リーダーがそこから先を
  読み捨てるためで、`output` が伸びなくなるのと同じ理由・同じ地点。Codex の PR レビューで
  「ライブ表示だけ末尾を流し続けるべきでは」と指摘されたが、**採らない**: 上の
  「チャンクの連結 = 最終出力」が崩れ、結果に載らないテキストを表示することになる。
  8MiB を超えた時点で結果自体が切り捨てられている (`truncated`)。

## セルの色 (TUI)

「このセッションで実行が終わったセル」を色で区別する (CYBERNEURA-DEV-567)。
待機=緑 / 実行中=黄 / 完了=青 / 失敗=赤で、完了・失敗はコマンド行も暗くする。

- **ファイルに結果が残っていることは「実行済み」ではない。** 昨日実行した文書を開いたときに
  全セルが完了色になっては、`a` (run all) がどこまで進んだかという肝心の情報が読めない。
  判定は `App::finished` (このセッションで実行したセルの index → 成否) だけで行う。
- **reload では `finished` を捨てる。** 再読み込み後の index は同じセルを指すとは限らない。

## 未実装 / 今後

- GUI で完了したセルを色分けする (今のところ TUI だけの機能)。

## 検証

```shell
cargo test                  # 118 件 (linux では 122 件。/proc を見るテストが 1 件、
                            #          DISPLAY を見るテストが 3 件ある)
cargo clippy --all-targets  # 警告ゼロを保つ
cargo fmt --all --check
```

手で動かす時は `examples/run-example.sh` を使う。`examples/exam.md` の使い捨てコピーを
gitignore 下 (`examples/scratch/`) に作ってからそれを実行するので、**何回動かしても
ワーキングツリーが汚れない**。実行は結果ファイルもドキュメントの隣に書くので、サンプルを
直接動かしてはいけない。引数はそのまま渡る。

```shell
examples/run-example.sh --run-all
RUNANDLOG=./target/release/runandlog examples/run-example.sh --list  # 既存のバイナリで
```

コピーを作るだけなら `examples/create-test-copy.sh` (パスを stdout に出す)。下の pty 起動の
ように、実行を自分で組み立てたい時に使う。

TUI は端末が必要なので自動テストの対象外。手で確認する場合は pty を割り当てて起動する。

```shell
DOC=$(examples/create-test-copy.sh)
# GNU (linux)。パスは環境変数で渡して内側で quote する (パスに空白が入ると壊れるため)
(sleep 1; printf 'q'; sleep 1) | DOC="$DOC" script -qec 'stty rows 30 cols 100; ./target/debug/runandlog "$DOC"' /dev/null
# BSD (macOS)。-c は無い
(sleep 1; printf 'q'; sleep 1) | script -q /dev/null ./target/debug/runandlog "$DOC"
```

Ctrl-C を送るなら `printf '\003'`。ただし **サンドボックス下では pty を割り当てられず
(`script: openpty: Operation not permitted`) TUI を起動できない**ので、その場合は手で確認する。
