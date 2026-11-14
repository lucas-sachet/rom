# rom

**Runtime Orchestrator for Microservices.** A local runtime that starts Node.js microservices on demand, stops them when they are idle, and keeps development machines light.

> Status: stage 3 is done (idle stop). Next is the file watcher. Plan, decisions and the real monorepo notes are in [docs/roadmap.md](docs/roadmap.md).

## Why

Running dozens of services with `nodemon` and `concurrently` means dozens of watchers and idle processes. `rom` runs one daemon per project that starts services only when they are needed, together with their dependencies.

`rom` does not merge services into one process. Each service keeps its own Node.js process, event loop and memory.

## Requirements

- Windows 10 or 11
- Rust 1.85+ (to build)
- Node.js on `PATH` (to run Node services)

Linux and macOS are not supported yet. The code uses Windows Job Objects and named pipes.

## Build

```powershell
cargo build --release -p rom
```

The binary is `target\release\rom.exe`. Put it on `PATH` to use it from any project.

## Quick start

In the root of your monorepo:

```powershell
rom init          # creates rom.yaml
rom check         # validates it
rom start --profile financeiro
rom status
rom logs financial-service --follow
rom stop --all
rom daemon stop
```

`rom start` launches the daemon in the background if it is not running.

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
    services: [financial-service]
```

Notes:

- `command` is run without a shell. Use `node` and pass the script in `args`. `.cmd` shims such as `npm`, `npx` or `ts-node` cannot be launched directly. For TypeScript with `ts-node`, use `command: node` with `args: [-r, ts-node/register, server.ts]`.
- The port in the yaml is the **public** port. The daemon's TCP gateway listens on `127.0.0.1` there and proxies to the process.
- Each service gets a free **listen** port as `PORT`, plus `ROM_SERVICE` and anything in `env:`. Node services also get `NODE_COMPILE_CACHE` (Node 22.1+, so 26 LTS), which keeps V8 bytecode between restarts.
- A connection to a sleeping service starts it (and its yaml dependencies) once; concurrent clients share that start.
- On-demand services stop after `idle_timeout` with no open proxied connections. Quiet keep-alives are closed after `connection_idle` and do not keep the service busy. `mode: persistent` never idle-stops.
- Design notes for the gateway, idle detection and shutdown are in [docs/decisions.md](docs/decisions.md).
- Service directories are used as-is. Each service keeps its own `node_modules`.
- Validation reports duplicate ports, unknown dependencies, dependency cycles, missing directories and unknown profile members.

## Commands

| Command | What it does |
|---|---|
| `rom init` | Creates `rom.yaml` in the current directory |
| `rom check` | Validates `rom.yaml` |
| `rom start [SERVICES] [-p PROFILE] [--all]` | Starts services and their dependencies |
| `rom stop [SERVICES] [-p PROFILE] [--all] [--force]` | Stops services. Refuses if running services depend on them, unless `--force` |
| `rom restart SERVICE` | Restarts one service; its dependencies keep running |
| `rom status` | Shows state, port, PID, uptime, restarts and last error |
| `rom logs SERVICE [-n N] [-f]` | Prints or follows a service log |
| `rom daemon start \| stop \| run` | Manages the background daemon (`run` stays in the foreground) |

## How it works

- The config file is found by walking up from the current directory, so every command works from anywhere inside the project. Use `--config <path>` to override.
- Each project has its own daemon, its own named pipe (`\\.\pipe\rom-<hash>`) and its own state in `.rom/` (logs, daemon log). Add `.rom/` to `.gitignore`.
- Every service runs inside a Windows Job Object. Stopping a service kills its whole process tree. If the daemon dies, the Job Object closes and no service is left running.
- A service is `running` once its listen port accepts connections (and its `/health` returns 2xx, if configured). Clients always connect to the public port through the gateway.

## Development

The proposal, what is already built, and the next stages are in [docs/roadmap.md](docs/roadmap.md). Locked technical choices are in [docs/decisions.md](docs/decisions.md).

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
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
