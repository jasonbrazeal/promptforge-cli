# promptforge-cli

Run [PromptForge](https://github.com/cppalliance/promptforge) prompts from a terminal.

```
promptforge run <file.md> [--args <text> | --args-file <path>] [--verbose]
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
```

## Usage

| Argument | Meaning |
| --- | --- |
| `<file.md>` | The prompt file to run |
| `--args <text>` | The run's input, exposed to the prompt as `args` |
| `--args-file <path>` | Read the run's input from a file instead; its contents become `args` verbatim. Use this for large inputs such as whole documents. |
| `-v`, `--verbose` | Print the configuration in use (gateway, model, prompt, args) and run progress to stderr |

- `--args` and `--args-file` are mutually exclusive; with neither, `args` is empty. Bare positional input is rejected.
- Output: the prompt's final text on stdout; errors on stderr.
- Exit status: 0 on success, 130 on Ctrl-C, 1 on any other failure.
- If the prompt asks for user input, the CLI reads one line from the terminal. With no terminal on stdin, input is reported unavailable.
- The `promptforge/web` capability (`search`, `fetch`) is provided, using the same gateway URL and key.

## Building

`cargo build`. The engine crates come from the [promptforge](https://github.com/cppalliance/promptforge) git repo; `Cargo.lock` pins the revision and `cargo update -p promptforge-api` moves it forward.

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