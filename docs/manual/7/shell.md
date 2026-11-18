# shell — a tour of nxsh, the shell you type at

## Description

`nxsh` is the shell at every login and in every terminal window. It runs programs and pipes one
into another as a Unix shell does, but what passes through a pipe is a typed value, a table most
often, not text. Its operators, such as `filter` and `sort`, work on that value in the shell
itself. It has variables, functions and control flow, as a scripting language does, and it
refuses to guess: a condition must be a Bool, and a number is not a string.

This page is a tour. Each example is what you type, after `>`, then what the shell prints. The
shell's tests type every example here and compare what comes back, in the order they appear, as
one session would; an example that runs a program shows what it printed on one machine, so a size
or a count on yours will differ. The rules are `shell-language(7)`; every operator, with an
example, is `shell-operators(7)`; and at the prompt, `help` lists the shell's own words and
`help NAME` says what one does.

## Running a command

A word at the start of a line names a program in `/bin`, and the words after it are its
arguments:

```nxsh
> whoami
```

```
user
alice
```

What a program sends back is a table: here one column, `user`, and one row. A path runs a file:
`./tool` runs `tool` in the working directory, and `/bin/whoami` is the same program as `whoami`.

## A pipeline

`|` sends what one stage gives to the next. Here `list` is a program, and `filter` and `select`
are operators, which the shell runs on the table `list` sent:

```nxsh
> list /bin | filter name == "man" | select name kind
```

```
name  kind
man   file
```

Nothing was turned into text on the way: `size` is still a number, so it compares and sorts as
one.

```nxsh
> list /bin | sort size --reverse | take 3 | select name size
```

```
name  size
nxsh  1203456
man   604104
page  523880
```

What a line leaves is shown when it ends. A table is shown in columns under its header, and any
other value as itself.

## Tables

A table's rows are records with the same fields, its columns. Four operators do most of the work:

- `filter` keeps the rows a predicate holds for: bare, as in `filter size > 500000`, it names
  the row's fields, and as a closure, `{ |row| ... }`, it names the row.
- `select` keeps the columns named.
- `sort` orders by each column named, `--reverse` to turn it round.
- `take N` keeps the first N rows; `skip N` and `last N` the others.

```nxsh
> list /bin | filter { |p| p.name ~= /^[cm]/ } | select name
```

```
name
copy
man
```

`~=` matches a regular expression, written between slashes. `count`, `sum`, `min`, `max` and `avg`
make a table one value:

```nxsh
> list /bin | filter size < 600000 | count
```

```
4
```

## Values and their types

The values are Ints, Floats, Strings, Bools, null, Lists, Records, Ranges and Tables, and the bytes
a program may send:

```nxsh
> [1, 2, 3] | sum
```

```
6
```

```nxsh
> {name: "ann", age: 30}.age + 1
```

```
31
```

`1..4` is 1, 2 and 3: a range stops short of its end. Nothing is converted for you: text that holds
a number is read with `parse`, and a condition must be a Bool, with no truthiness:

```nxsh
> ("41" | parse Int) + 1
```

```
42
```

```nxsh
> "41" + 1
```

```
nxsh: cannot apply `+` to String and Int
```

Arithmetic that overflows, and division by zero, are errors too, not a value made up.

### Strings and quoting

A string is in double quotes, with `\n`, `\t`, `\"` and `\\` inside it. There is no
interpolation: `format` fills each `{}` in turn, and `++` joins two strings.

```nxsh
> let name = "ann"
> format("hello, {}: {} new", name, 3)
```

```
hello, ann: 3 new
```

```nxsh
> "hello, " ++ name
```

```
hello, ann
```

A program's arguments are words, and need no quotes unless they hold a space: `list /bin` and
`list "/storage/my stick"` both work. A path anywhere but a line's start is a word too.

### Variables

`let` binds a name that nothing can assign to, though a later `let` may bind it anew; `mut` makes
one that can be assigned; and `const` one that cannot even be bound again where it is, and that a
script may offer to others with `pub`.

```nxsh
> let a = 1
> a = 2
```

```
nxsh: `a` was bound with `let` — only a `mut` binding can be assigned to
```

```nxsh
> let a = 2
> a
```

```
2
```

```nxsh
> const LIMIT = 10
> const LIMIT = 20
```

```
nxsh: `LIMIT` is a const and cannot be rebound in the same scope
```

```nxsh
> mut total = 0
> for n in [3, 4, 5] { total = total + n }
> total
```

```
12
```

The environment is a record, `$env`: `$env.PWD` is the working directory, and `$env.HOME` your
home. `cd` changes `$env.PWD`, and only to somewhere that exists. At the prompt, `$last` is the
last line that succeeded: `$last.value` what it computed, and `$last.status` how the stages of its
pipelines ended, null when it ran none. A failed line's stages are `e.stages`, in a `catch`.

## Blocks and control flow

`if`, `for`, `while` and `match` take blocks in braces, and a block is a value: its last line's.

```nxsh
> let size = if 7 > 5 { "big" } else { "small" }
> size
```

```
big
```

`for` walks a table's rows, a list's values or a range's numbers:

```nxsh
> for n in 1..4 { display n * n }
```

```
1
4
9
```

`match` takes the first arm whose pattern fits: a range, a type, a record's shape, or `_` for
anything.

```nxsh
> match 7 {
>     0..5 => "few"
>     _ => "many"
> }
```

```
many
```

`break` leaves a loop and `continue` goes on to its next turn. A line ending in `|`, or with a
brace or a bracket open, goes on to the next, as the `match` above did.

## Errors and try

A failing stage, or a failing expression, stops the line, and the shell says why. `try` gives a
value to fall back on, and its `catch` block sees the error:

```nxsh
> try { "x" | parse Int } catch (e) { e.message }
```

```
cannot parse "x" as Int
```

`fail` raises an error of your own, for a function to refuse what it was given:

```nxsh
> def half(n) { if n % 2 != 0 { fail "an odd number" } else { n / 2 } }
> try { half(3) } catch (e) { e.message }
```

```
an odd number
```

A pipeline whose stage fails is reported failed, naming the stage, and the programs after it still
finish what they were sent; a program that failed and sent nothing ends it there. In a `catch`,
`e.stages` says how each stage ended. `strict { ... }` stops the rest at the first that fails.
`Ctrl-C` stops what is running: the stages a pipeline started, or a loop at its next turn.

## Functions and scripts

`def` names a function. It is called with parentheses, and in a pipe `_` is where the value goes:

```nxsh
> def area(w: Int, h: Int) -> Int { w * h }
> area(3, 4)
```

```
12
```

```nxsh
> 5 | area(_, 2)
```

```
10
```

A closure, `{ |x| x * 2 }`, is a function with no name, which `map`, `filter` and `each` take.

A script is a file of lines, run as `nxsh FILE` or by its path, `./build.nx`; it reads its
arguments as `$args`. `use` takes the functions another script offers with `pub`:

```text
# /home/alice/lib.nx
pub def twice(n) { n * 2 }
```

```nxsh
> use "./lib.nx" { twice }
> twice(21)
```

```
42
```

## At the prompt

| Keys | What they do |
|---|---|
| Left, Right, Home, End | Move along the line |
| Backspace, Delete | Erase before the cursor, or under it |
| `Ctrl-U`, `Ctrl-W` | Erase back to the line's start, or a word |
| Up, Down | An older or a newer line from this session |
| `Ctrl-R` | Search back through them |
| Tab | Complete a command's name, or a path |
| `Ctrl-C` | Drop the line, or stop what is running |
| `Ctrl-D` | At an empty line, leave |

History is kept for the session, and a new login starts without it. `help` lists the operators,
the builtins and the keywords; `man` reads this manual, and a page longer than the screen is shown
a page at a time.

## Doing what needs a grant

A session's programs read the system and change your own files. A program that changes the
machine, such as `disk --mount`, `service --restart` or `account --add`, needs a grant, and fails
naming `with` when it has none. `with` asks the view broker to run it in a view that has the
grant, asking for your password when the policy says to:

```text
> with admin disk --mount /dev/blk/1
> with power shutdown
```

`with --list` lists the views you may use. Which views there are, what each grants and who may use
them is the machine's policy, `views.toml(5)`.

## See also

`shell-operators(7)`, `shell-language(7)`, `man(1)`, `page(1)`
