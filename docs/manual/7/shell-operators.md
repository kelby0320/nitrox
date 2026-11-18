# shell-operators — the shell's operators, each with an example

## Description

An operator works on the value a pipe carries, in the shell itself, where a program runs as a
process of its own: in `list /bin | filter size > 100000 | count`, `list` is a program, and
`filter` and `count` are operators. What passes between them is a value, typed: a table, a list,
a record, a string or a number.

An operator's arguments are its own. A bareword names a column, as in `sort size`; a predicate is
a comparison or a closure, `filter size > 100` or `filter { |it| it.size > 100 }`; and `--reverse`
is a flag. Each example below is run by the shell's tests, and prints what is shown under it.
`help NAME` says the same of one operator at the prompt.

## Sequences

A table's rows, a list's values, a string's characters and a range's values are each a sequence, and
each comes back in the shape it went in: a string filtered is a string.

### filter

`filter PREDICATE`

Keep what a predicate holds for: a comparison, or a closure.

```
> [{n: 1}, {n: 5}, {n: 9}] | filter n > 2
[{ n: 5 }, { n: 9 }]
```

### select

`select COLUMN...`

Keep only the columns named, in the order named.

```
> [{a: 1, b: 2, c: 3}] | select c a
c  a
3  1
```

### sort

`sort [COLUMN...] [--reverse]`

Order by each column named in turn, or by the values themselves.

```
> ["fig", "pear", "apple"] | sort --reverse
["pear", "fig", "apple"]
```

### take

`take N`

The first N.

```
> "nitrox" | take 3
nit
```

### skip

`skip N`

All but the first N.

```
> "nitrox" | skip 3
rox
```

### last

`last N`

The last N.

```
> 1..10 | last 3
[7, 8, 9]
```

### dedupe

`dedupe`

Drop each one equal to one before it, keeping the order.

```
> [1, 2, 1, 3, 2] | dedupe
[1, 2, 3]
```

### each

`each { |it| ... }`

Run a closure on each, for what it does, and pass them on unchanged.

```
> [1, 2] | each { |n| display n * 10 }
10
20
[1, 2]
```

### map

`map { |it| ... }`

Each made what the closure returns; records make a table.

```
> 1..4 | map { |n| {n: n, square: n * n} }
n  square
1  1
2  4
3  9
```

## Reducing

Many values made one. Of nothing, `count` and `sum` are 0, and the rest are errors.

### count

`count`

How many there are.

```
> "nitrox" | count
6
```

### sum

`sum [COLUMN]`

The total of a column, or of the values.

```
> [{size: 3}, {size: 4}] | sum size
7
```

### min

`min [COLUMN]`

The least: strings in order, and mixed types refused.

```
> ["pear", "fig"] | min
fig
```

### max

`max [COLUMN]`

The greatest, as `min` orders.

```
> [2, 9, 4] | max
9
```

### avg

`avg [COLUMN]`

The mean, a Float even of Ints.

```
> [1, 2] | avg
1.5
```

### reduce

`reduce [--from SEED] { |acc, it| ... }`

Fold them into one value, starting from the first or from SEED.

```
> ["a", "b", "c"] | reduce --from ">" { |acc, it| acc ++ it }
>abc
```

## Strings

Text taken apart and put together. `upper` and `lower` change ASCII letters only.

### split

`split SEPARATOR`

A string cut at each separator, as a list.

```
> "a,b,c" | split ","
["a", "b", "c"]
```

### join

`join SEPARATOR`

A list's values as one string, the separator between.

```
> ["a", "b", "c"] | join "-"
a-b-c
```

### trim

`trim`

Without leading and trailing whitespace.

```
> "  x  " | trim
x
```

### replace

`replace FROM TO`

Every FROM made TO, as written: not a pattern.

```
> "a-b-c" | replace "-" "+"
a+b+c
```

### upper

`upper`

In capitals.

```
> "Nitrox" | upper
NITROX
```

### lower

`lower`

In small letters.

```
> "Nitrox" | lower
nitrox
```

### capture

`capture /PATTERN/`

The match and each group's, as a list, or null if it does not match.

```
> "10-20" | capture /(\d+)-(\d+)/
["10-20", "10", "20"]
```

### format

`format(TEMPLATE, VALUE...)`

Each {} filled with the next value; in a pipe, its value first.

```
> format("{} of {}", 3, 4)
3 of 4
```

## Records

A record's fields, in the order its schema has them.

### keys

`keys`

A record's field names.

```
> {name: "a", size: 3} | keys
["name", "size"]
```

### values

`values`

A record's values.

```
> {name: "a", size: 3} | values
["a", 3]
```

### merge

`merge RECORD`

Two records as one, RECORD's value kept where both have a field.

```
> {a: 1, b: 2} | merge {b: 3, c: 4}
{ a: 1, b: 3, c: 4 }
```

## Numbers

A Float made an Int, each named by which way it loses information.

### round

`round`

To the nearest Int, a half away from zero.

```
> 2.5 | round
3
```

### floor

`floor`

To the Int at or below.

```
> -1.5 | floor
-2
```

### ceil

`ceil`

To the Int at or above.

```
> 1.2 | ceil
2
```

### trunc

`trunc`

To the Int toward zero.

```
> -1.7 | trunc
-1
```

### abs

`abs`

Without its sign.

```
> -3 | abs
3
```

## In and out

Files read and written, and a value shown. `save` and `display` end a chain.

### open

`open PATH...`

Files read as one stream: a .tsm as itself, a .txt a line a row.

```
> 1..4 | map { |n| {n: n} } | save ./n.tsm
> open ./n.tsm | sum n
6
```

### save

`save PATH`

The value written to PATH: a table as .tsm, or a line a row as .txt.

```
> ["one", "two"] | save ./words.txt
> open ./words.txt
line
one
two
```

### display

`display [VALUE]`

Show a value, a table in columns, as the prompt shows what is left.

```
> [1, 2] | display
[1, 2]
```

## See also

`shell-language(7)`, the language whole.
