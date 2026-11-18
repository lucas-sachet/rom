# rom

**Runtime Orchestrator for Microservices.** A local runtime that starts Node.js microservices on demand, stops them when they are idle, and keeps development machines light.

> Status: stage 8 is done (Linux/macOS). Plan, decisions and the real monorepo notes are in [docs/roadmap.md](docs/roadmap.md).

**New here?** [Install + first steps](docs/getting-started.md).

## Why

Running dozens of services with `nodemon` and `concurrently` means dozens of watchers and idle processes. `rom` runs one daemon per project that starts services only when they are needed, together with their dependencies.

`rom` does not merge services into one process. Each service keeps its own Node.js process, event loop and memory.

## Requirements

- Windows 10/11, Linux, or macOS
- Rust 1.85+ (to build)
- Node.js on `PATH` (to run Node services)

On Windows the daemon uses Job Objects and named pipes. On Unix it uses process groups and a socket under `$XDG_RUNTIME_DIR` (or the system temp dir).

## Install and first run

Build, put `rom` on `PATH`, create or discover a `rom.yaml`, then start a profile. Full walkthrough: [docs/getting-started.md](docs/getting-started.md).

```powershell
cargo build --release -p rom
# put target/release/rom(.exe) on PATH, then in your monorepo:
rom discover --write
rom check
rom up --profile financeiro   # stays in this terminal; Ctrl+C stops everything
```

`rom up` is the usual day-to-day command. `rom start` still exists for a detached background daemon.

## Configuration (`rom.yaml`)

```yaml
version: 1

project:
  name: tex-erp
  root: .

runtime:
  startup_timeout: 30s
  shutdown_timeout: 30s
  idle_timeout: 30s             # stop on-demand services with no proxied connections
  connection_idle: 5s           # close keep-alive with no bytes either way
  # What `rom up` prints besides each service's own log.
  # dev: service logs only. Rom stays quiet; a failed command still prints its error.
  # status: also lifecycle lines — starting, sleeping (idle standby), restarting, failed.
  # verbose: status, plus daemon diagnostics for tracking bugs.
  # `ROM_LOG` overrides the diagnostics when it is set.
  log: dev
  # Color only the [service] prefix in `rom up`, like concurrently.
  # The letters are colored. `prefix_background` fills the background instead,
  # and only applies when prefix colors are on.
  # Prefixes share one width: the longest name in this run, capped at 20.
  # The cap is used only when a name is longer than that. Shorter names are
  # padded; longer ones compact to `sta..end`. The log text itself is not colored.
  # Has no effect when stdout is not a terminal or when NO_COLOR is set.
  # Uncomment to enable:
  # prefix_colors: true
  # prefix_background: true

watch:
  debounce: 300ms               # min time from first change before restart
  settle: 500ms                 # quiet window after last change before restart

services:
  user-service:
    path: ./user-service
    command: node
    args: [server.js]
    port: 3001                  # public port (gateway); process gets a free PORT
    health:                     # optional; without it, "ready" means the listen port accepts connections
      type: http
      path: /health
    dependencies: []

  financial-service:
    path: ./financial-service
    command: node
    args: [server.js]
    port: 3002
    mode: on-demand             # or `persistent` for queues/cron (never idle-stopped)
    dependencies:
      - user-service

profiles:
  financeiro:
    services:
      - financial-service
      - user-service:user          # optional terminal label; no space around `:`
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
- A profile entry `service:label` (no space around `:`) shows `label` in that tag. Without `:label`, the service name is shown. `rom start` and `rom stop` still use the real service name. The label follows the same width rule.
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

User-facing install and first steps: [docs/getting-started.md](docs/getting-started.md). Proposal and history: [docs/roadmap.md](docs/roadmap.md). Locked technical choices: [docs/decisions.md](docs/decisions.md).

```powershell
cargo test --workspace      # unit tests and integration tests (they run real Node processes)
cargo clippy --workspace --all-targets -- -D warnings
```

Layout:

```text
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
