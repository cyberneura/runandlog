# Run and Log

Markdown に書かれたシェルコマンドを選んで実行し、実行結果をその Markdown に書き戻すツール。

AI エージェントがユーザーにコマンドの実行を依頼し、ユーザーが実行し、エージェントが結果を確認する、
というやりとりを 1 つのファイルの上で完結させるために作っている。IPython Notebook のように
「コマンドのセル」と「その結果」が並んだ状態を、ただの Markdown として保つ。

## インストール

```shell
cargo install --path crates/runandlog-cli
```

`runandlog` バイナリが入る。

## 使い方

```shell
runandlog exam.md              # TUI で開く
runandlog exam.md --list       # セルの一覧を表示する
runandlog exam.md --run 2      # 2 番目のセルだけ実行する (繰り返し指定可)
runandlog exam.md --run-all    # 全セルを順に実行する
```

### オプション

| オプション | 既定値 | 説明 |
|---|---|---|
| `--max-inline-lines <N>` | 50 | 出力がこの行数を超えたら別ファイルに書き出す |
| `--shell <PATH>` | 環境変数 `SHELL` | コマンドを渡すシェル |
| `--cwd <DIR>` | Markdown のあるディレクトリ | コマンドの作業ディレクトリ |
| `--timeout <SECONDS>` | 無制限 | 1 セルあたりの実行時間の上限。超えるとプロセスグループごと終了させる |
| `--gui` | - | デスクトップアプリで開く (未実装) |

### TUI のキー操作

| キー | 動作 |
|---|---|
| `j` / `k` (`↓` / `↑`) | セルの選択を移動する |
| `Enter` / `r` | 選択中のセルを実行する |
| `a` | 全セルを順に実行する |
| `g` / `G` | 先頭 / 末尾のセルへ |
| `PageUp` / `PageDown` | スクロール |
| `R` | ファイルを読み直す |
| `q` / `Esc` / `Ctrl-C` | 終了 (コマンドの実行中は、その完了を待ってから終了する) |

## Markdown の書き方

### 実行されるブロック

情報文字列が `shell` / `sh` / `bash` / `zsh` のフェンスドコードブロックがセルになる。
それ以外の言語のブロックは実行対象にならない。

````markdown
```shell
date
```
````

1 つのブロックに複数行書いた場合は、まとめて 1 回のシェル起動で実行される。

````markdown
```shell
ls /opt
ls /tmp
```
````

### 実行結果

実行すると、ブロックの直後に結果ブロックが書き込まれる。

````markdown
```shell
date
```

<!-- runandlog:begin -->
Ran result: 2026-08-14 09:53:32 (exit 0, 0.02s, 1 lines)

```text
Fri Aug 14 09:53:32 JST 2026
```
<!-- runandlog:end -->
````

結果は HTML コメントのマーカーで挟む。Markdown として妥当で表示もされず、再実行のときに
前回の結果を確実に置き換えられる。何度実行しても結果ブロックは 1 つだけ残る。

### 結果を別ファイルに書き出す

出力が `--max-inline-lines` (既定 50 行) を超えると、`<Markdown 名>-result-<セル番号>.txt` に
書き出され、Markdown にはそのリンクだけが載る。

行数にかかわらず別ファイルにしたい場合は、書き出し先をあらかじめ指定できる。指定方法は 2 つある。

ブロックの直後に `Result:` 段落を置き、リンクで書き出し先を書く方法:

````markdown
```shell
date
```

Result:
[date-command-result.txt](date-command-result.txt)
````

フェンスの情報文字列に `out=` を書く方法:

````markdown
```shell out=date-command-result.txt
date
```
````

書き出し先は Markdown のあるディレクトリの配下に限られる。絶対パスや `..` を含む指定は
無視され、通常どおり (インラインまたは自動採番の別ファイルに) 書き出される。

## 構成

| クレート | 役割 |
|---|---|
| `crates/runandlog-core` | Markdown のパース、コマンドの実行、結果の整形。ファイル IO を持たない純粋なコア |
| `crates/runandlog-cli` | `runandlog` バイナリ。CLI と TUI、ファイルの読み書き |

GUI (デスクトップアプリ) は今後 `runandlog-core` を共有する形で追加する。パース仕様が
分岐しないよう、コアは必ず共有する。

## 開発

```shell
cargo test          # 全テスト
cargo clippy --all-targets
cargo fmt --all
```

`examples/exam.md` が動作確認用のサンプル。**実行すると書き換わる**ので、コピーして試すこと。

```shell
cp examples/exam.md /tmp/exam.md
cargo run -p runandlog-cli -- /tmp/exam.md
```
