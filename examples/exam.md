# Sample document

Open this with `runandlog examples/exam.md` and you can pick and run the code
blocks below.

## A single-line command

```shell
date
```

## A multi-line command

Commands written together in one block are run together, in a single shell
invocation.

```shell
ls /opt
ls /tmp
```

## Sending the result to a separate file

To write to a separate file regardless of how long the output is, put a
`Result:` paragraph right after the block and give the destination as a link.

```shell
seq 1 5
```

Result:
[exam-seq-result.txt](exam-seq-result.txt)

## Designating it with a fence attribute

The fence info string works too, instead of a `Result:` paragraph.

```shell out=exam-uname-result.txt
uname -a
```

## When the output is long

Output beyond `--max-inline-lines` (50 by default) goes to a separate file
automatically, with no designation needed.

```shell
seq 1 60
```
