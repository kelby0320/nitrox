# sleep — wait for a while

## Synopsis

```
sleep DURATION
```

## Description

`sleep` waits for DURATION, then exits. A bare number is seconds. The suffixes `ns`, `us`, `ms`,
`s`, `m` and `h` are accepted, with an optional fraction. A duration of zero returns at once.

`sleep` can be interrupted: when it is asked to stop it exits at once, and that is success. A
duration it cannot read is an error, not a sleep of no time.

## Options

- `--help`: show the usage and exit.
- `--version`: show the version and exit.

`sleep` takes no other option.

## Output

Nothing. The exit status is the whole result.

## Exit status

0 after the wait, or when it was asked to stop. 1 when the timer could not be made or the wait did
not complete. 2 for an option it does not know, an operand count other than one, or a duration it
cannot read.

## Examples

```
sleep 5        # five seconds
sleep 250ms    # a quarter of a second
sleep 1.5m     # a minute and a half
```

## See also

`date(1)`, `nxsh(1)`
