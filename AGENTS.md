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

## リリース

`scripts/release.sh` (既定 minor) が version を上げて push し、GitHub Actions の Release
ワークフローを起動して watch する。ワークフローは **workflow_dispatch のみ**で、push では走らない。

- **version は毎回上げる。** 公開済みの version で起動すると plan ジョブが即失敗する
  (mac の署名と公証を無駄に回さないよう、ビルド前に見ている)。
- **push した後に失敗すると、リリースされない version が main に残る。** 次に普通に実行すると
  さらに上の version を振ってしまうので、スクリプトはその状態を検出して止まる。
  `scripts/release.sh --retry` が同じ version でやり直す。
- **watch する run は、dispatch ごとの合言葉 (`dispatch_id` → `run-name`) と push した SHA の
  両方で特定する。** 「最新の run」を見ると、今回の run がまだ登録されていない時に**前回の結果**を
  今回のものとして報告する。合言葉が `displayTitle` に出ない場合に備えて、SHA と run id の
  大小によるフォールバックも持たせてある。
- **未リリースの version が残っているかは、自分が書いた bump コミット (`Release v<version>`) で
  判定する。** リリース履歴では区別が付かない — 0 件なら「初回」と「初回が失敗して残った」が
  同じに見え、1 件でもあれば別 version の draft が「過去に出した証拠」に見える。
- **mac は arm64 のみ、既定 feature (GUI 込み) でビルドして Developer ID で署名 + 公証する。**
  Homebrew の cask は quarantine を付けるので、署名も公証も無いと受け取った側の初回起動が
  止まる。素のバイナリは staple できないので、チケットは Apple 側にしか残らない
  (Gatekeeper はオンラインで引く)。**公証の結果は `--output-format json` の `status` で見る。**
  提出が完了しさえすれば `notarytool` は成功終了しうるので、終了コードだけでは Invalid を拾えない。
- **Linux は `--no-default-features`。** 既定 feature には Tauri が入り、webkit2gtk に
  動的リンクしたバイナリは同じ webkit を持たない環境で起動しない。ただし glibc には
  動的リンクしたままなので、**「どの Linux でも動く」わけではない** (ビルドした runner より
  古い glibc では起動しない)。
- **Release は draft で作ってからアセットを数えて公開する。** `gh release create` は先に
  Release を作ってからアップロードするので、途中で失敗すると**公開済みで中身の欠けた Release**
  が残る。draft で残っている間は誰にも見えず、同じ version の再実行が**それを消して作り直す**
  (公開済みの Release には触らない)。
- Homebrew の cask は別リポジトリ (`cyberneura/homebrew-tap`) なので、CI ではなく
  `scripts/update-cask.sh` が手元の認証情報で更新する。tap への書き込み権を持つ token を
  この repo の secret に置かずに済む。**cask の `binary` はアーカイブ内のディレクトリを含めた
  パスで書く** (cask はトップレベルのディレクトリに降りてくれない)。
- crates.io は `--crates` を指定した時だけ。公開は取り消せないので既定は off。
  `CARGO_REGISTRY_TOKEN` の secret が要る。**publish ジョブは macOS で走らせる**:
  `cargo publish` は公開するものをビルドして検証し、既定 feature には webview が要るため。
  publish 済みの version は飛ばすので、片方だけ通った後の再実行が続きから進められる。
  `Cargo.toml` の `runandlog-core = { path = ..., version = ... }` は workspace version と
  揃っている必要がある (揃っていないと publish が拒否される)。release.sh が両方書き換える。

## 未実装 / 今後

- 実行中の出力のライブ表示。現在の `exec::run` は終了時にまとめて `String` を返すだけで、
  途中経過を渡すフックが無い。

## 検証

```shell
cargo test                  # 99 件 (linux では 103 件。/proc を見るテストが 1 件、
                            #         DISPLAY を見るテストが 3 件ある)
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
