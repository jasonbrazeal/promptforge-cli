# promptforge-cli

Run [PromptForge](https://github.com/cppalliance/promptforge) prompts from a terminal.

```
promptforge run <file.md> [--args <text> | --args-file <path>] [--input <name>=<path>]... [--output <name>=<path>]... [--verbose]
```

## Configuration

Everything comes from the environment. A gateway must already be running.

| Variable | Required | Meaning |
| --- | --- | --- |
| `PROMPTFORGE_GATEWAY_URL` | yes | The gateway's API root, e.g. `http://127.0.0.1:8081/v1` |
| `PROMPTFORGE_GATEWAY_API_KEY` | yes | The gateway's shared bearer (`[server] api_key` in its config) |
| `PROMPTFORGE_MODEL` | no | Model id bound to every model role the prompt declares; defaults to the first model in the gateway catalog. |

See the [promptforge repo](https://github.com/cppalliance/promptforge) for more information.

## Example

```bash
export PROMPTFORGE_GATEWAY_URL=http://127.0.0.1:8081/v1
export PROMPTFORGE_GATEWAY_API_KEY=change-me-to-a-secret
./target/debug/promptforge run /path/to/hello.promptforge.md

# A prompt that declares `input: paper.md` and `output: report.json`:
./target/debug/promptforge run /path/to/analyze.promptforge.md \
  --args '{"id": "P2300R10"}' \
  --input paper.md=/path/to/p2300r10.md \
  --output report.json=/path/to/p2300r10.report.json
```

## Usage

| Argument | Meaning |
| --- | --- |
| `<file.md>` | The prompt file to run |
| `--args <text>` | The run's input, exposed to the prompt as `args` |
| `--args-file <path>` | Read the run's input from a file instead; its contents become `args` verbatim. |
| `--input <name>=<path>` | Before the run, copy the file at `<path>` into the run's store as `<name>`, so the prompt can `store.read("<name>")`. Repeatable. Use this for documents the prompt declares under `input:`. |
| `--output <name>=<path>` | After a successful run, copy the store file `<name>` the prompt wrote with `store.write` to `<path>`. Repeatable. Use this for files the prompt declares under `output:`. |
| `-v`, `--verbose` | Print the configuration in use (gateway, model, prompt, args, inputs, outputs) and run progress to stderr |

- `--args` and `--args-file` are mutually exclusive; with neither, `args` is empty. Bare positional input is rejected.
- Output: the prompt's final text on stdout; errors on stderr.
- Exit status: 0 on success, 130 on Ctrl-C, 1 on any other failure.
- If the prompt asks for user input, the CLI reads one line from the terminal. With no terminal on stdin, input is reported unavailable.
- The `promptforge/web` capability (`search`, `fetch`) is provided, using the same gateway URL and key.

## Files in and out: `--input` and `--output`

A prompt has no access to the filesystem. Its Lua runs in a sandbox with no `io` library, and the `store` table it reads and writes (`store.read`, `store.write`, `store.exists`, ...) is a private in-memory store that exists only for the duration of one run. Everything the prompt reads must be put into that store before the run starts, and everything it writes is gone when the run ends unless something copies it out. `--input` and `--output` are those two copies.

**`--input <name>=<path>`** runs before the prompt starts. The CLI reads the file at `<path>` and writes its contents into the store under `<name>`. The prompt then reads it with `store.read("<name>")`. The prompt never sees `<path>`.

**`--output <name>=<path>`** runs after the prompt finishes successfully. The CLI reads the store file `<name>` — which the prompt must have written with `store.write("<name>", ...)` — and writes it to `<path>` on disk. If the prompt never wrote `<name>`, the run is reported as an error naming the missing store file. Nothing is extracted after a failed or cancelled run.

Both flags are repeatable, and each takes exactly one `<name>=<path>` pair. The first `=` splits the pair, so a host path may itself contain `=`.

### Matching the prompt's declarations

A prompt documents the store files it expects and produces in its frontmatter:

```yaml
input:
  path: paper.md
  description: The document to analyze
output:
  path: report.json
  description: The analysis, as JSON
```

The `<name>` you pass to `--input` must be the `input.path` the prompt declares, and the `<name>` for `--output` must be its `output.path`. The declarations are documentation for you, the caller; the CLI does not read them, so it cannot warn about a mismatch. If you seed `--input document.md=...` and the prompt reads `paper.md`, the prompt's `store.read` fails with a not-found error inside the run. Read the prompt's frontmatter and use the names it declares.

A well-behaved prompt checks for its input up front and returns a usable message when it is missing:

```lua
if not store.exists("paper.md") then
  return "input error: the store has no paper.md; seed it with --input paper.md=<path>"
end
```

### Names and paths

- `<name>` is a store path: relative, forward slashes only, no `..` segments, no backslashes. A name that breaks these rules is rejected when arguments are parsed, before anything runs.
- `<path>` is an ordinary host path, absolute or relative to the current directory.
- Every `--input` file is read before the gateway is contacted, so a missing or unreadable input fails fast with no network round trip.
- `--output` overwrites an existing file at `<path>`.
- `--verbose` prints each mapping as `input:   <name> <- <path> (<bytes> bytes)` and `output:  <name> -> <path>` so you can see what was seeded and where results will land.

### Worked example

A prompt that reads one file and writes its contents twice:

````markdown
---
name: double
description: Write the input twice to the output
promptforge: 0
input:
  path: input.txt
  description: Any text file
output:
  path: output.txt
  description: The input, twice
---

# Double

## Double

```lua
local text = store.read("input.txt")
store.write("output.txt", text .. text)
return "wrote output.txt: " .. (#text * 2) .. " bytes"
```
````

Run it:

```bash
printf 'hello\n' > /tmp/in.txt

promptforge run double.promptforge.md \
  --input input.txt=/tmp/in.txt \
  --output output.txt=/tmp/out.txt

cat /tmp/out.txt
```

stdout is `wrote output.txt: 12 bytes` (the prompt's return value), and `/tmp/out.txt` contains `hello` on two lines. Note that `--input` and `--output` are independent of `--args`: `args` is a string the prompt reads as a value, while the store holds files the prompt reads and writes by name. A prompt can use both, for example `--args '{"id": "P2300R10"}'` for a document number and `--input paper.md=...` for the document itself.

## Building

`cargo build`. The engine crates come from the [promptforge](https://github.com/cppalliance/promptforge) git repo; `Cargo.lock` pins the revision and `cargo update -p promptforge-api-runtime` moves it forward.

The engine's crates depend on `workspace-hack`, its cargo-hakari feature-unification crate, which from outside that workspace would pull every product's dependencies (Tauri, GTK, candle, ...) into this build. The `[patch]` in `Cargo.toml` swaps it for the empty stub in `workspace-hack/`; nothing there needs maintenance unless upstream renames the crate or bumps its version.

Before committing:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

Minimum supported Rust: 1.89, the floor the dependency tree imposes. Bumping it is a minor release.

## License

Distributed under the Boost Software License 1.0. See [LICENSE](LICENSE).