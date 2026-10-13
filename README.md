# tty-sim

`tty-sim` is a synchronous terminal-automation library and command-line tool for
driving one interactive program through a POSIX pseudo-terminal (PTY). It executes
a strict YAML script sequentially, streams the child's byte output, waits for raw
output, and masks presentation bytes without changing what expectations observe.

tty-sim handles OSC 11 background-color and CSI 6n cursor-position queries inside
the child PTY. Query bytes remain visible to raw `expect` matching but are not
forwarded to stdout.

## Command line

Build with the stable Rust toolchain and run:

```text
cargo run -- --script demo.yaml --command /bin/sh --
```

The executable syntax is:

```text
tty-sim --script PATH --command PROGRAM [--rows N] [--cols N]
        [--seed N] [--grace DURATION] [--termination-grace DURATION]
        [--] [ARG ...]
```

`PROGRAM` is executed directly. Arguments after `--` are passed literally.
Durations use integer milliseconds (`250ms`) or integer/decimal seconds (`2s`,
`1.5s`). Logging goes to stderr, defaults to `WARN`, and is configured through
`RUST_LOG`.

For script sequencing diagnostics, enable debug logging:

```text
RUST_LOG=tty_sim=debug cargo run -- --script demo.yaml --command bash
```

Debug events identify command starts and completions and show whether an
expectation matched retained history or waited for new output. They include raw
buffer offsets and byte counts, but never pattern or command contents. The
`tty_sim=trace` level additionally reports PTY reader chunk sizes, buffer
truncation, and terminal-protocol response sizes.

## YAML scripts

A script is one YAML mapping with a required `steps` sequence. `cwd`, `env`, and
`masks` are optional:

```yaml
cwd: fixtures/app

env:
  TERM: xterm-256color
  AWS_PROFILE: demo
  AWS_SECRET_ACCESS_KEY: null

masks:
  - secret-password
  - from: 192.168.1.100
    to: 10.0.0.1

steps:
  - expect: "$ "
  - send: "clear\n"
  - expect: "$ "
  - wait: 500ms
  - typing: { min: 20ms, max: 50ms }
  - type: "gh auth status\n"
  - expect: "$ "
  - simulate:
      command: "ghst login -p developer\n"
      output: |-
        DEVICE AUTHORIZATION REQUIRED
        Waiting for authorization in browser...
  - show: "walkthrough complete\r\n"
```

Relative `cwd` values resolve from tty-sim's execution directory. The directory is
validated before allocating a PTY. Environment strings replace inherited values;
`null` or `~` removes an inherited variable. Root masks are active before the first
child byte is read. A scalar mask is replaced with the same number of `*` bytes.

Each step has exactly one command key:

| Step | Meaning |
| --- | --- |
| `type: TEXT` | Send each UTF-8 character using the current typing range. |
| `typing: { min: D, max: D }` | Change the range for later `type` and `simulate` steps. |
| `key: KEY` | Send a supported key sequence immediately. |
| `send: BYTES` | Send bytes immediately. |
| `show: BYTES` | Emit bytes only to the presentation. |
| `wait: DURATION` | Wait for a duration. |
| `drain: DURATION` | Clear expectation history, wait for a fixed period, then clear history again. |
| `expect: BYTES` | Wait up to five seconds for raw PTY bytes. |
| `expect: { pattern: BYTES, timeout: D }` | Wait with an explicit timeout. |
| `mask: TEXT` / `mask: { from: BYTES, to: BYTES }` | Mask future presentation. |
| `simulate: { command: TEXT, output: BYTES }` | Type into the presentation, then emit output without writing to the child. |

### Expect buffering and command sequencing

`expect` searches unconsumed raw PTY history before it waits for new output. This
is deliberate: a child can respond to `send`, `key`, or `type` before the next
`expect` starts, and searching buffered bytes prevents that fast response from
being missed. A successful match consumes history through the first match; later
bytes remain available to subsequent expectations. Masks do not change raw
history, while `show` and `simulate` do not enter it.

Startup output is history too. When driving an interactive shell, consume its
initial prompt before sending the first command. Then use one input/expectation
pair per prompt:

```yaml
steps:
  - expect: "$ "
  - send: "sleep 3\n"
  - expect: "$ "
  - send: "echo 'Hello, world!'\n"
  - expect: "$ "
  - show: "This appears after the echo command.\n"
  - send: "exit\n"
```

Do not send several shell commands and assume one later prompt identifies the
last command. Each command can produce its own matching prompt. Likewise, omitting
the initial `expect` can make a later prompt expectation match the retained startup
prompt immediately.

Use `drain` to abandon buffered output before starting a new interaction:

```yaml
steps:
  - drain: 250ms
  - send: "echo 'Hello, world!'\n"
  - expect: "$ "
```

`drain: 250ms` clears existing expectation history and consumes all output
received during a fixed 250 ms period. Output is still displayed, with masks and
terminal-query handling applied normally. The period uses the same duration format
as `wait`, including `250ms` and `1.5s`; `drain: 0ms` clears history immediately.
Incoming output does not restart the period.

Place `drain` before `send`, `key`, or `type`. Draining after input can consume
the response that the next `expect` should match. Only output received by tty-sim
before the final clear is consumed; delayed output can still arrive afterward.
Draining does not establish that the child is ready or that earlier commands have
finished. Expecting a known prompt or completion marker establishes that boundary.

The initial typing range is 10–200 ms. Both `min` and `max` are required in a
`typing` step, and `min` must not exceed `max`.

Ordinary YAML strings are UTF-8. Byte-valued fields also accept an explicit hex
mapping, including nested pattern and mask values:

```yaml
steps:
  - send: { hex: "001bff" }
  - expect: { pattern: { hex: "ff" }, timeout: 1s }
```

Hex contains an even number of ASCII hexadecimal digits with no prefix or
separators. YAML performs all string escaping. tty-sim does not interpret a second
escape language, normalize line endings, or append Enter/newline bytes. In
particular, `|` retains its final newline while `|-` removes it.

Scripts reject unknown or duplicate fields, multiple command keys in one step,
multiple YAML documents, anchors, aliases, merge keys, and explicit tags. The old
line-oriented script format is not supported.

Supported named keys are ASCII-case-insensitive:

```text
Enter Tab Backspace Escape Esc
Up Down Right Left Home End Insert Delete PageUp PageDown
F1 F2 F3 F4 F5 F6 F7 F8 F9 F10 F11 F12
```

A single printable ASCII character is also a key. `Ctrl+A` through `Ctrl+Z`,
`Alt+KEY`, and `Ctrl+Alt+A` through `Ctrl+Alt+Z` are supported. Unsupported or
noncanonical modifier combinations fail closed.

## Exit codes

| Code | Meaning |
| ---: | --- |
| 0 | Help, or successful commands followed by natural child exit with status 0. |
| 1 | Invalid invocation or engine configuration. |
| 2 | Script read, YAML schema, script configuration, or validation failure. |
| 3 | PTY or child spawn failure. |
| 4 | Command execution failure. |
| 5 | PTY reader failure. |
| 6 | Presentation sink failure. |
| 7 | Completion or cleanup failure. |
| 8 | Commands succeeded, but completion required `SIGTERM`. |
| 9 | Commands succeeded, but completion required `SIGKILL`. |
| 10 | Child failure cannot be represented by the offset scheme. |
| 101–255 | Child exited with status 1–155; the code is `100 + child status`. |

## Library use

`parse_script` returns the whole parsed `Script`, including spawn configuration,
initial masks, and commands:

```rust,no_run
use tty_sim::{Engine, SpawnSpec, parse_script};

let script = parse_script("steps:\n  - send: \"exit\\n\"\n")?;
let mut spec = SpawnSpec::new("/bin/sh");
spec.env = script.env().to_vec();
spec.current_dir = script.cwd().map(ToOwned::to_owned);

let mut engine = Engine::builder(spec)
    .initial_masks(script.masks().to_vec())
    .seed(7)
    .build()?;
engine.execute(&script.commands())?;
let completion = engine.finish()?;
assert!(completion.status().success());
# Ok::<(), Box<dyn std::error::Error>>(())
```

Always consume an engine with `finish()`, `wait_for_exit()`, or `terminate()`.
Dropping an unfinished engine performs only best-effort cleanup.

## Architecture

The codebase is organized strictly around feature-owned hexagonal boundaries:

- **`automation`**: Owns the externally meaningful command language, typing range model, session workflow, lifecycle management, and outbound contracts (`TerminalBackend`, `TerminalControl`, `ChildHandle`, `Sleeper`, `DelaySource`).
- **`presentation`**: Owns the raw history buffer, streaming expectation matcher, leftmost-longest byte masker, terminal protocol query filter and responder, and the outbound `OutputSink` contract.
- **`terminal`**: Owns POSIX PTY allocation (`portable-pty`), process group management (`nix`), signaling (`killpg`), and child reaping.
- **`script`**: Owns strict YAML parsing (`serde-saphyr`), schema DTOs, grammar parsing, and source location mapping.
- **`cmd`**: Owns the CLI driving adapter (`argh`), stderr logging setup, diagnostic formatting, and exit code policy.
- **`lib.rs`**: Narrow library facade and library composition boundary wiring production adapters into the public [`Engine`](file:///home/martin/code/tty-sim/src/lib.rs) facade.

See `DESIGN.md` for the behavioral and architectural specification.

## Development

Before submitting a change, run `cargo fmt`, `cargo check`, `cargo clippy`, and
`cargo test`. See `DESIGN.md` for the behavioral specification.

## Platform status

Linux is implemented and verified. macOS is intended but not yet verified. Other
POSIX-like systems supported by the PTY backend are best effort. Windows and
ConPTY are unsupported.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
