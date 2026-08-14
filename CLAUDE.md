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
  だけを担当し、ファイルの読み書きは `runandlog-cli` の `session` が行う。TUI・非対話実行・
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

## 未実装 / 今後

- **GUI (Tauri デスクトップアプリ)**。`--gui` は現在エラーを返すだけ。
  作る場合も `runandlog-core` を必ず共有すること (パース仕様の分岐を避けるため)。
  なお Bot (Amedeo) の実行環境には webkit2gtk / gtk3 が無く Tauri をビルドできないため、
  GUI の実装は動作確認ができる環境で行う必要がある。

## 検証

```shell
cargo test                  # 64 件
cargo clippy --all-targets  # 警告ゼロを保つ
cargo fmt --all --check
```

TUI は端末が必要なので自動テストの対象外。手で確認する場合は pty を割り当てて起動する。

```shell
(sleep 1; printf 'q'; sleep 1) | script -qec "stty rows 30 cols 100; ./target/debug/runandlog /tmp/exam.md" /dev/null
```
