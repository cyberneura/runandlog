# 動作確認用のサンプル

`runandlog examples/exam.md` で開くと、下のコードブロックを選んで実行できる。

## 1 行のコマンド

```shell
date
```

## 複数行のコマンド

1 つのブロックにまとめて書いたコマンドは、1 回のシェル起動でまとめて実行される。

```shell
ls /opt
ls /tmp
```

## 結果を別ファイルに書き出す

出力の行数にかかわらず別ファイルへ書き出したい場合は、ブロックの直後に `Result:` 段落を置き、
リンクで書き出し先を指定する。

```shell
seq 1 5
```

Result:
[exam-seq-result.txt](exam-seq-result.txt)

## フェンスの属性で指定する

`Result:` 段落の代わりに、フェンスの情報文字列でも指定できる。

```shell out=exam-uname-result.txt
uname -a
```

## 出力が長い場合

`--max-inline-lines` (既定 50) を超えた出力は、指定が無くても自動で別ファイルに書き出される。

```shell
seq 1 60
```
