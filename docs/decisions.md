# Decisions

## Gateway and idle time

The gateway is a TCP proxy on `127.0.0.1`. Each yaml `port` is public; the process listens on an ephemeral port passed as `PORT`. The gateway starts a sleeping service (and its declared dependencies) on first connection; concurrent clients share one start via `ensure_ready`.

A service stays busy while any proxied connection is open, which covers HTTP requests, Server-Sent Events and WebSockets. After `idle_timeout` (default 30s) with zero connections, an on-demand service is stopped; `mode: persistent` is never idle-stopped.

Idle HTTP keep-alive connections do not count. Node keeps them open by default, so treating them as activity would mean a service never goes idle. The gateway closes a proxied connection after `connection_idle` (default 5s) with no bytes in either direction, then the service idle timer can run.

## Shutdown

Each service is started with its own hidden console (`CREATE_NEW_CONSOLE`). Stop re-invokes a small helper (`rom __ctrl_break <pid>` / `rom-ctrl-break`) that attaches to that console and sends `CTRL_BREAK` (Node: `SIGBREAK`), then waits up to `shutdown_timeout` before `TerminateJobObject`. Attaching in-process would deliver the signal to the daemon itself. Closing the Job Object when the daemon exits still kills leftovers.

`CREATE_NEW_PROCESS_GROUP` cannot be combined with `CREATE_NEW_CONSOLE` (Windows ignores the group flag), so the break is sent to every process on that console — normally just the service.

There is no `rom` npm package. Services that already drain on `SIGTERM` / `SIGBREAK` / `SIGINT` work unchanged. On Linux and macOS the pair will be `SIGTERM` then `SIGKILL`.

## Watcher

One `notify` watcher per project. Directories are registered non-recursively and ignored names (`node_modules`, `.git`, `dist`, …) are skipped while walking, so Windows never attaches a subtree watch under those trees. Sleeping services are not started by file changes. Presence of `.git/index.lock` pauses reloads until the lock is gone.

## Node

The target runtime is Node.js 26 LTS. `rom` sets `NODE_COMPILE_CACHE` per service so repeat starts reuse V8 bytecode.
