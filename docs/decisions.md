# Decisions

## Gateway and idle time

The gateway is a TCP proxy on `127.0.0.1`. Each yaml `port` is public; the process listens on an ephemeral port passed as `PORT`. The gateway starts a sleeping service (and its declared dependencies) on first connection; concurrent clients share one start via `ensure_ready`.

A service stays busy while any proxied connection is open, which covers HTTP requests, Server-Sent Events and WebSockets. After `idle_timeout` (default 30s) with zero connections, an on-demand service is stopped; `mode: persistent` is never idle-stopped.

Idle HTTP keep-alive connections do not count. Node keeps them open by default, so treating them as activity would mean a service never goes idle. The gateway closes a proxied connection after `connection_idle` (default 5s) with no bytes in either direction, then the service idle timer can run.

## Shutdown

On Windows each service is started with its own console (`CREATE_NEW_CONSOLE`) created already hidden (`SW_HIDE`) so no prompt window flashes. Stop re-invokes a small helper (`rom __ctrl_break <pid>` / `rom-ctrl-break`, itself with `CREATE_NO_WINDOW`) that attaches to that console and sends `CTRL_BREAK` (Node: `SIGBREAK`), then waits up to `shutdown_timeout` before `TerminateJobObject`. Attaching in-process would deliver the signal to the daemon itself. Closing the Job Object when the daemon exits still kills leftovers.

Day-to-day use is `rom up`: the daemon stays in the current terminal, service logs are tailed into that terminal, and Ctrl+C stops everything. `rom start` still launches a detached background daemon for when you want the CLI to return.

`CREATE_NEW_PROCESS_GROUP` cannot be combined with `CREATE_NEW_CONSOLE` (Windows ignores the group flag), so the break is sent to every process on that console — normally just the service.

On Unix the service is started in a new process group. Stop sends `SIGTERM` to the process, then `SIGKILL` to the group after `shutdown_timeout`. Dropping the process-tree handle also SIGKILLs the group.

Services do not import a rom library. A process that already drains on `SIGTERM` / `SIGBREAK` / `SIGINT` works unchanged. The CLI is the binary; the intended public install is an npm package that puts that binary on `PATH`.

## Discovery

`rom discover` only suggests a yaml. It never overwrites `rom.yaml` unless `--write --force` is passed. Dependencies are left empty — local package graphs are not turned into runtime start edges automatically. Each package directory with its own entrypoint stays its own service; similar code is never merged into one process. The checked-in / confirmed `rom.yaml` remains the source of truth for the registry.

## Metrics

Status reports working set and cumulative CPU time per service process, plus the same for the daemon as a separate line. Working set on Windows is not exclusive physical RAM (shared pages, overcommit), so values must never be summed and labelled as total RAM. Active proxied connections and seconds since last connection open/close (or start) are included so idle behaviour is visible without inventing a fake memory total.

## Platforms

Windows remains the primary target. Linux and macOS share the same gateway and yaml: services run in their own process group, IPC is a Unix domain socket under `$XDG_RUNTIME_DIR` (or the temp dir), and stop sends `SIGTERM` then force-kills the process group. The Windows path stays Job Object + named pipe + `CTRL_BREAK` via the helper.

## Watcher

One `notify` watcher per project. Directories are registered non-recursively and ignored names (`node_modules`, `.git`, `dist`, …) are skipped while walking, so Windows never attaches a subtree watch under those trees. Sleeping services are not started by file changes. Presence of `.git/index.lock` pauses reloads until the lock is gone.

## Node

The target runtime is Node.js 26 LTS. `rom` sets `NODE_COMPILE_CACHE` per service so repeat starts reuse V8 bytecode.
