# Decisions

## Gateway and idle time

The gateway is a TCP proxy. A service stays busy while any proxied connection is open, which covers HTTP requests, Server-Sent Events and WebSockets.

Idle HTTP keep-alive connections do not count. Node keeps them open by default, so treating them as activity would mean a service never goes idle. The gateway closes a proxied connection after a configurable period with no bytes in either direction (initial value: 5s) and only then starts the idle timer.

Not built yet. The process manager starts and stops services only when asked.

## Shutdown

Stop kills the process tree immediately through a Windows Job Object.

There is no `SIGTERM` on Windows, so a Unix-style graceful stop is not available. The Windows equivalent for Node is a hidden console plus `CTRL_BREAK`, which Node surfaces as `SIGBREAK`. On Linux and macOS the equivalent is `SIGTERM`, then `SIGKILL` after the grace period.

That path is deferred until the gateway tracks open connections. Until then there is nothing to drain, and an immediate kill matches what a dev restart needs. Services will not get a `rom` npm package; the ones that already drain on `SIGTERM` / `SIGBREAK` will work unchanged when graceful stop lands.

## Node

The target runtime is Node.js 26 LTS. `rom` sets `NODE_COMPILE_CACHE` per service so repeat starts reuse V8 bytecode.
