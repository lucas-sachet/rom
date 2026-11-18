# rom

**Runtime Orchestrator for Microservices.** A local runtime that starts Node.js microservices on demand, stops them when they are idle, and keeps development machines light.

> Status: Windows, Linux and macOS are supported. Design decisions are in [docs/decisions.md](docs/decisions.md).

**New here?** [Install + first steps](docs/getting-started.md).

## Why

Running dozens of services with `nodemon` and `concurrently` means dozens of watchers and idle processes. `rom` runs one daemon per project that starts services only when they are needed, together with their dependencies.

`rom` does not merge services into one process. Each service keeps its own Node.js process, event loop and memory.

## Requirements

- Windows 10/11 x64, Linux x64 or arm64, or macOS x64 or arm64
- Node.js 18+ to install `rom` and to run Node services

Rust 1.85+ is only needed to build rom from source. On Windows the daemon uses Job Objects and named pipes. On Unix it uses process groups and a socket under `$XDG_RUNTIME_DIR` (or the system temp dir).

## Install and first run

The npm package is `rom-cli` (`rom` is already another package). The command it installs is `rom`. Full walkthrough: [docs/getting-started.md](docs/getting-started.md).

```powershell
npm install -g rom-cli
rom --version
# then, in your project:
rom discover --write
rom check
rom up --profile app          # stays in this terminal; Ctrl+C stops everything
```

`rom up` is the usual day-to-day command. `rom start` still exists for a detached background daemon.

## Configuration (`rom.yaml`)

The annotated example is [`examples/rom.yaml`](examples/rom.yaml). Copy that file to your project root and edit the names, paths and ports. The directories in it are placeholders, so `rom check` reports them missing until they exist. rom only reads `rom.yaml` or `rom.yml`, and it does not load this copy from the repository.

```yaml
version: 1

project:
  name: my-app
  root: .

services:
  api:
    path: ./services/api
    command: node
    args: [server.js]
    port: 3001

profiles:
  app:
    services:
      - api
```

Notes:

- `command` is run without a shell. Use `node` and pass the script in `args`. `.cmd` shims such as `npm`, `npx` or `ts-node` cannot be launched directly. For TypeScript with `ts-node`, use `command: node` with `args: [-r, ts-node/register, server.ts]`.
- The port in the yaml is the **public** port. The daemon's TCP gateway listens on `127.0.0.1` there and proxies to the process.
- Each service gets a free **listen** port as `PORT`, plus `ROM_SERVICE` and anything in `env:`. Node services also get `NODE_COMPILE_CACHE` (Node 22.1+, so 26 LTS), which keeps V8 bytecode between restarts.
- A connection to a sleeping service starts it (and its yaml dependencies) once; concurrent clients share that start.
- On-demand services stop after `idle_timeout` with no open proxied connections. Quiet keep-alives are closed after `connection_idle` and do not keep the service busy. `mode: persistent` never idle-stops.
- One project watcher restarts **running** services when their files change (`node_modules`, `.git`, `dist`, … are not watched). Sleeping services stay asleep. A `.git/index.lock` pauses reloads.
- `runtime.log` chooses what rom prints beside service logs. `dev` (the default) shows service logs only. `status` also prints lifecycle lines: a service starting again, going to sleep after idle (standby), restarting, failing, or crashing. `verbose` adds daemon diagnostics for tracking bugs. `ROM_LOG` overrides those diagnostics when set. Service `mode` (`on-demand` or `persistent`) is a different setting.
- `runtime.prefix_colors` (off unless uncommented) paints only the `[service]` tag in `rom up`. The letters use basic ANSI colors, so your terminal theme sets the exact shade. Each service in a run gets its own color, repeating only after the 12th. `runtime.prefix_background` fills that tag's background with the same hues, and only applies while prefix colors are on. The shared width is the longest name in that run, capped at 20: the cap applies only when a name is longer than 20. Shorter names are padded and longer ones compact to `sta..end`, so the log text always starts in the same column. The log line is printed as the process wrote it.
- A profile entry `service:label` (no space around `:`) shows `label` in that tag. In the example, `web:site` prints `site`. Without `:label`, the service name is shown. `rom start` and `rom stop` still use the real service name. The label follows the same width rule.
- Design notes for the gateway, idle detection and shutdown are in [docs/decisions.md](docs/decisions.md).
- Service directories are used as-is. Each service keeps its own `node_modules`.
- Validation reports duplicate ports, unknown dependencies, dependency cycles, missing directories and unknown profile members.

## Commands

| Command | What it does |
|---|---|
| `rom init` | Creates `rom.yaml` in the current directory |
| `rom discover [--write] [--force]` | Scans the monorepo and suggests a `rom.yaml` (prints by default; `--write` saves, `--force` overwrites) |
| `rom check` | Validates `rom.yaml` |
| `rom up [SERVICES] [-p PROFILE] [--all]` | Runs the daemon in this terminal, streams logs, optionally starts services (Ctrl+C stops all) |
| `rom start [SERVICES] [-p PROFILE] [--all]` | Starts services via a background daemon (returns immediately) |
| `rom stop [SERVICES] [-p PROFILE] [--all] [--force]` | Stops services. Refuses if running services depend on them, unless `--force` |
| `rom restart SERVICE` | Restarts one service; its dependencies keep running |
| `rom status` | Shows daemon cost plus per-service state, port, PID, uptime, connections, restarts, working set, CPU, last activity and last error |
| `rom logs SERVICE [-n N] [-f]` | Prints or follows a service log |
| `rom daemon start \| stop \| run` | Manages the background daemon (`run` stays in the foreground without auto-start) |

## How it works

- The config file is found by walking up from the current directory, so every command works from anywhere inside the project. Use `--config <path>` to override.
- Each project has its own daemon, its own IPC endpoint (`\\.\pipe\rom-<hash>` on Windows, `$XDG_RUNTIME_DIR/rom-<hash>.sock` on Unix) and its own state in `.rom/` (logs, daemon log). Add `.rom/` to `.gitignore`.
- Every service runs in an owned process tree (Job Object on Windows, process group on Unix). Stop sends `CTRL_BREAK` / `SIGTERM`, waits `shutdown_timeout`, then force-kills the tree if needed. If the daemon dies, leftover services are reaped with the tree.
- A service is `running` once its listen port accepts connections (and its `/health` returns 2xx, if configured). Clients always connect to the public port through the gateway.

## Development

User-facing install and first steps: [docs/getting-started.md](docs/getting-started.md). Locked technical choices: [docs/decisions.md](docs/decisions.md).

```powershell
cargo test --workspace      # unit tests and integration tests (they run real Node processes)
cargo clippy --workspace --all-targets -- -D warnings
```

Build from source when you are changing rom itself:

```powershell
cargo build --release -p rom
# target/release/rom.exe on Windows, target/release/rom on Linux and macOS
```

### Releasing

Tagged releases build the binaries in GitHub Actions and publish them. People who install rom do not need Rust.

1. On npm, create a granular access token that can publish packages, without a 2FA prompt for CI. Add it as the `NPM_TOKEN` Actions secret.
2. Set `version` in the workspace `Cargo.toml` to the release you want. The template in `npm/rom-cli/package.json` should use the same number.
3. Commit and push `main`. The tag has to point at a commit that already contains `.github/workflows/release.yml`.
4. Tag that version and push the tag. `v0.1.0` is the first one, matching the current `Cargo.toml`.

```powershell
git tag v0.1.0
git push origin v0.1.0
```

The workflow builds Windows x64, Linux x64 (musl), Linux arm64 (musl), macOS x64 and macOS arm64. It creates a GitHub Release with those binaries and publishes `rom-cli` plus one optional package per platform. `npm install -g rom-cli` installs only the package for the current machine and exposes the `rom` command.

A manual run of the Release workflow (Actions → Release → Run workflow) only builds. It publishes when the run comes from a version tag.

Layout:

```text
.github/workflows/release.yml  tag build, GitHub Release, npm publish
npm/rom-cli/         package template; the command name is rom
examples/rom.yaml    annotated config; copy it to your project (not loaded here)
apps/rom/            CLI and daemon binary
crates/rom-core/     config, registry, process manager, gateway, IPC, Job Object wrapper
crates/rom-core/tests/lifecycle.rs   process lifecycle with real Node processes
crates/rom-core/tests/gateway.rs     TCP gateway and on-demand start
crates/rom-core/tests/idle.rs        idle stop and quiet keep-alives
crates/rom-core/tests/watcher.rs     file watcher restarts
crates/rom-core/tests/graceful.rs    CTRL_BREAK then Job Object fallback
crates/rom-core/tests/discover.rs    monorepo scan → suggested yaml
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
