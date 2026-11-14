# Decisions

## Gateway and idle time

The gateway is a TCP proxy on `127.0.0.1`. Each yaml `port` is public; the process listens on an ephemeral port passed as `PORT`. The gateway starts a sleeping service (and its declared dependencies) on first connection; concurrent clients share one start via `ensure_ready`.

A service stays busy while any proxied connection is open, which covers HTTP requests, Server-Sent Events and WebSockets. After `idle_timeout` (default 30s) with zero connections, an on-demand service is stopped; `mode: persistent` is never idle-stopped.

Idle HTTP keep-alive connections do not count. Node keeps them open by default, so treating them as activity would mean a service never goes idle. The gateway closes a proxied connection after `connection_idle` (default 5s) with no bytes in either direction, then the service idle timer can run.

## Shutdown

Stop kills the process tree immediately through a Windows Job Object.

There is no `SIGTERM` on Windows, so a Unix-style graceful stop is not available. The Windows equivalent for Node is a hidden console plus `CTRL_BREAK`, which Node surfaces as `SIGBREAK`. On Linux and macOS the equivalent is `SIGTERM`, then `SIGKILL` after the grace period.

That path is deferred until the gateway tracks open connections. Until then there is nothing to drain, and an immediate kill matches what a dev restart needs. Services will not get a `rom` npm package; the ones that already drain on `SIGTERM` / `SIGBREAK` will work unchanged when graceful stop lands.

## Watcher

One `notify` watcher per project. Directories are registered non-recursively and ignored names (`node_modules`, `.git`, `dist`, …) are skipped while walking, so Windows never attaches a subtree watch under those trees. Sleeping services are not started by file changes. Presence of `.git/index.lock` pauses reloads until the lock is gone.

## Node

The target runtime is Node.js 26 LTS. `rom` sets `NODE_COMPILE_CACHE` per service so repeat starts reuse V8 bytecode.
