# Sample document

Open a throwaway copy of this with `examples/run-example.sh` and you can pick and
run the code blocks below. Running this file where it sits rewrites it and drops
result files beside it, which is what the script is there to avoid.

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

## Stopping a command

This one counts to ten, a second at a time. Interrupt it partway -- Ctrl-C in the
TUI or from a `--run-all`, **Stop** in the desktop app -- and the numbers it had
reached are written back with the result marked `cancelled`.

```shell
for i in $(seq 1 10); do echo "$i"; sleep 1; done
```
