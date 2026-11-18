# Getting started with rom

Install the CLI, point it at a monorepo, then start only the services you need.

## Installation

### Requirements

- **OS:** Windows 10/11 x64 or Linux x64 or arm64 for the npm package. On macOS, build from source.
- **Node.js:** 18 or newer. This installs the `rom` command and runs your services.

Rust is only required if you build rom from source.

### Install the command

```powershell
npm install -g rom-cli
rom --version
```

The package name is `rom-cli` because `rom` already belongs to another package. The command is still `rom`. npm downloads a prebuilt binary for your platform. You do not compile anything. The package is on npm after a version tag is published. The first tag is `v0.1.0`.

If `rom` is not found, the npm global bin directory is not on `PATH`. `npm prefix -g` prints the prefix; the binaries are in its `bin` folder (on Windows, next to `npm` itself).

### Build from source

Requires [Rust](https://rustup.rs/) 1.85 or newer.

```powershell
git clone https://github.com/lucas-sachet/rom.git
cd rom
cargo build --release -p rom
```

Binary locations:

| OS | Path |
|---|---|
| Windows | `target\release\rom.exe` |
| Linux / macOS | `target/release/rom` |

### Put `rom` on PATH

**Windows (PowerShell, current user):**

```powershell
$bin = Resolve-Path .\target\release
[Environment]::SetEnvironmentVariable(
  "Path",
  $env:Path + ";" + $bin,
  "User"
)
# Open a new terminal, then:
rom --version
```

Or copy `rom.exe` somewhere already on PATH (for example a tools folder you maintain).

**Linux / macOS:**

```bash
mkdir -p ~/.local/bin
cp target/release/rom ~/.local/bin/
# ensure ~/.local/bin is on PATH, then:
rom --version
```

Building from source is for working on rom. After you change it, stop any running daemon or `rom up` and replace the binary. An old process keeps the old binary. People using `npm install -g rom-cli` do not need this step.

### Verify

```powershell
rom --help
```

You should see subcommands such as `init`, `discover`, `start`, `status`, and `daemon`.

---

## First steps

Work from the root of the project you want to orchestrate (the directory that should own `rom.yaml`).

The field-by-field guide is [`examples/rom.yaml`](../examples/rom.yaml), with generic names (`my-app`, `api`, `worker`, `web`). Copy it and edit the paths:

```powershell
cp path\to\rom\examples\rom.yaml .\rom.yaml
```

The paths in that file are placeholders. `rom check` reports a missing directory until you point each service at a real folder.

### 1. Create or discover `rom.yaml`

**Empty template** (edit services by hand):

```powershell
rom init
```

**Scan an existing Node monorepo** (suggested yaml; does not overwrite unless you ask):

```powershell
rom discover              # print suggestion to stdout
rom discover --write      # write rom.yaml if it does not exist
rom discover --write --force   # replace an existing rom.yaml
```

`discover` leaves `dependencies:` empty on purpose. Add start edges yourself after you know which services must come up together.

Add `.rom/` to `.gitignore` if it is not already ignored (logs and daemon state live there).

### 2. Fill in services

Minimal service entry:

```yaml
services:
  api:
    path: ./services/api
    command: node
    args: [server.js]
    port: 3001
```

Rules that matter on day one:

- `command` is **not** a shell. Prefer `node` with the script in `args`. Do not use `npm` / `npx` / `ts-node` shims as `command`.
- TypeScript with `ts-node`: `command: node`, `args: [-r, ts-node/register, server.ts]`.
- `port` is the **public** port clients use. The process receives a free listen port as `PORT`.
- Optional: `dependencies`, `mode: persistent`, and `health: { type: http, path: /health }`.

Validate before starting anything:

```powershell
rom check
```

### 3. Start something (prefer `rom up`)

Stay in the current terminal with live logs (Ctrl+C stops everything):

```powershell
rom up api                    # one service (+ its yaml dependencies)
rom up --profile app
rom up --all
rom up                        # daemon only; services start on first request to their public port
```

Background mode (CLI returns immediately; output goes to `.rom/logs/`):

```powershell
rom start api
rom start --profile app
rom daemon stop
```

If a background daemon is already running, stop it before `rom up`.

### 4. Hit the public port

Clients always talk to the port in `rom.yaml` (for example `http://127.0.0.1:3001`). The gateway proxies to the process.

If the service was sleeping, the first connection starts it (and its dependencies) once. Concurrent clients share that start.

```powershell
curl http://127.0.0.1:3001/
```

### 5. Inspect what is running

```powershell
rom status
rom logs api
rom logs api --follow
```

`status` shows the daemon on its own line, then each service (state, PID, connections, working set, CPU, last activity). Working set is **not** exclusive RAM — do not add the numbers up and treat the sum as physical memory.

### 6. Stop cleanly

```powershell
rom stop api
rom stop --all
rom daemon stop          # stops the daemon and any leftover services
```

On-demand services also stop by themselves after `idle_timeout` (default 30s) with no open proxied connections. Quiet HTTP keep-alives are closed after `connection_idle` (default 5s) so they do not keep a service awake forever.

---

## Everyday loop

Typical session:

```powershell
cd path\to\your-monorepo
rom up --profile app
# logs stream here; develop against the public ports in rom.yaml
# Ctrl+C when done
```

Useful habits:

- Run commands from any subdirectory; `rom` walks up to find `rom.yaml`. Override with `--config path\to\rom.yaml`.
- Edit a running service’s files: the project watcher restarts that service after debounce/settle. Sleeping services stay asleep.
- After rebuilding `rom` itself, stop any running daemon/`rom up` before testing again — an old process keeps the old binary.

## Next reading

- Full `rom.yaml` reference and command table: [README](../README.md)
- Why the gateway, idle timer, and shutdown work the way they do: [decisions.md](decisions.md)