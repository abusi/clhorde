# Changelog

## [0.5.0](https://github.com/abusi/clhorde/compare/clhorde-v0.4.0...clhorde-v0.5.0) (2026-04-01)


### Features

* **daemon:** generate session ID upfront and pass --session-id to claude CLI ([dee2fbb](https://github.com/abusi/clhorde/commit/dee2fbbf09b2538f2db8f67d5c5fc7e50d62c200)), closes [#58](https://github.com/abusi/clhorde/issues/58)
* display session ID in CLI, TUI, and web interface ([25aa611](https://github.com/abusi/clhorde/commit/25aa61138675ddb8685ff95460702ac6fab90a06))


### Bug Fixes

* **web:** use REST response to update prompt list immediately ([85b9e51](https://github.com/abusi/clhorde/commit/85b9e515c110657b59463ad3b6ef522e9e7235c7)), closes [#59](https://github.com/abusi/clhorde/issues/59)


### Documentation

* move web interface plan to docs/plan/ with numbered prefix ([a404907](https://github.com/abusi/clhorde/commit/a4049077256030525c7546836f6f90d5f7a6612b))


### Tests

* **daemon:** add tests for session ID generation and resume flow ([e0f8398](https://github.com/abusi/clhorde/commit/e0f8398db6b05f45d86c1e1fcf61d61066ebe42e))


### Continuous Integration

* add Renovate configuration for automated Rust dependency updates ([4978abc](https://github.com/abusi/clhorde/commit/4978abc23f7b6df0a76a3167f10654cf9d9c9ec1))


### Miscellaneous Chores

* Add MIT License to the project ([dfc528e](https://github.com/abusi/clhorde/commit/dfc528ef2c8c97c9d218d9a24b81bba4364f0361))

## [0.4.0](https://github.com/abusi/clhorde/compare/clhorde-v0.3.0...clhorde-v0.4.0) (2026-03-28)


### Features

* **daemon:** add structured logging to all daemon modules ([800746b](https://github.com/abusi/clhorde/commit/800746b738ce7889e0974e9ce12edb790656c0c3))
* **tui:** add mouse wheel and keyboard scrollback for PTY output ([6462920](https://github.com/abusi/clhorde/commit/64629205830a6d704eb2f21ce5500c8ce400272e))
* **web:** add clhorde-web crate with REST API for daemon state (M1–M3) ([c0767c8](https://github.com/abusi/clhorde/commit/c0767c8bae7c11bc45bb5411cf6240a145164afd))
* **web:** add CORS support, frontend login flow, and PTY resize forwarding ([82453ad](https://github.com/abusi/clhorde/commit/82453adb86b0432f935d7107d362c5ce29df7f4e))
* **web:** add daemon bridge IPC client and shared app state ([b236310](https://github.com/abusi/clhorde/commit/b2363103be6b944900f1e9850dae19df1f8263c4))
* **web:** add dashboard HTML/CSS scaffold, WS client, and state management (E2-M1, E2-M2) ([1290a6a](https://github.com/abusi/clhorde/commit/1290a6a6bf8c55ce11e43a7be4014f3aecdb8443))
* **web:** add prompt detail view with ANSI output, xterm.js terminal, follow-up input, and action controls (E3) ([f641277](https://github.com/abusi/clhorde/commit/f641277d0571d0a78f1a7aa4a3dad844df74066a))
* **web:** add prompt list view and submission form (E2-M3, E2-M4) ([dcafd5c](https://github.com/abusi/clhorde/commit/dcafd5c1b6e340ee0cae071c7bfdfadc07c27e8d))
* **web:** add PTY byte streaming over WebSocket with per-prompt subscriptions (M7) ([3673978](https://github.com/abusi/clhorde/commit/3673978788313ef087c546d0d365a5472c55692a))
* **web:** add REST API config and store endpoints (M5) ([aa3fd6a](https://github.com/abusi/clhorde/commit/aa3fd6acb4e7f9fbd3398c86e7ba870bc69a5065))
* **web:** add REST API prompt action endpoints (M4) ([a388366](https://github.com/abusi/clhorde/commit/a38836632b7bb2b3bf203b9ba8abf6d5fe4107d8))
* **web:** add static file serving with embedded assets and SPA fallback (M8) ([6b9a9b9](https://github.com/abusi/clhorde/commit/6b9a9b9d8533211d2e3cd8b4d92454aad20ae9e2))
* **web:** add store management, error UX, token auth, and workspace integration (E4) ([369cb1e](https://github.com/abusi/clhorde/commit/369cb1e18d9d6aedcb866d55c233f7191831ff3e))
* **web:** add WebSocket handler with event fan-out (M6) ([ecadd1c](https://github.com/abusi/clhorde/commit/ecadd1c53d3e04f1c9e653979120cf5e9c1e085f))
* **web:** scaffold clhorde-web crate with CLI args and health endpoint ([f7b68ab](https://github.com/abusi/clhorde/commit/f7b68abeed301593afdf31c6d6ca9822b2f0ab9f))
* **web:** upgrade xterm.js from v5.5.0 to v6.0.0 with WebGL addon ([9cf71df](https://github.com/abusi/clhorde/commit/9cf71dfc33f1ea05cbaf8d6601bc40f6381bed35))


### Bug Fixes

* **web:** decode PTY bytes as Uint8Array to fix UTF-8 rendering artifacts ([70bbdfa](https://github.com/abusi/clhorde/commit/70bbdfaf9d039b3ed083f4a2651049ee9cc1ecb1))
* **web:** improve xterm terminal font and size ([c8b4d51](https://github.com/abusi/clhorde/commit/c8b4d516f978e46cf93e62f876529b278ec4658d))
* **web:** normalize prompt status case for filtering, kill button, and badges ([e7510b2](https://github.com/abusi/clhorde/commit/e7510b2e68fdaa18be0a219a4adf30ec52169809))
* **web:** prevent login overlay from showing when auth is disabled ([ccff38e](https://github.com/abusi/clhorde/commit/ccff38e4eadcd2eaa77829c01d33bf6b132e5dcb))
* **web:** suppress dead_code warnings for incremental bridge API ([a36c0d4](https://github.com/abusi/clhorde/commit/a36c0d43d414a989b6aa532bfc1224b18a33baea))


### Code Refactoring

* **web:** move JS tests out of static/ to avoid embedding in binary ([9ecd299](https://github.com/abusi/clhorde/commit/9ecd299f739967f9ee9948763bcbf18cd0cf0eb0))


### Documentation

* add HTTP server implementation plan ([8f89799](https://github.com/abusi/clhorde/commit/8f89799ceea09d29e5d12073080340d093aa05ef))
* add systemd user service setup for clhorded ([4480504](https://github.com/abusi/clhorde/commit/44805042c8c8ff9613e14c72548e27459c3fa5ed)), closes [#40](https://github.com/abusi/clhorde/issues/40)
* add web interface and HTTP bridge plan ([f09a9ad](https://github.com/abusi/clhorde/commit/f09a9adb20ebcdd1ea82b0728eedd743d0a6a7bf))
* add web interface documentation page ([5a63e2b](https://github.com/abusi/clhorde/commit/5a63e2bc276974895ee9e6651ffecb329f1192f0))
* clean up plan folder ([#52](https://github.com/abusi/clhorde/issues/52)) ([1fe62da](https://github.com/abusi/clhorde/commit/1fe62dac9485b485b04349df46b318c274342c9c))
* mark E2-M5 and E2-M6 complete (already implemented in E2-M1/M2) ([09ce536](https://github.com/abusi/clhorde/commit/09ce536ef97dd537caaa8f9bf1252b5bd85998f2))
* mark plan 15 (daemon logging) as done ([d81b581](https://github.com/abusi/clhorde/commit/d81b5815cd4d1e5402e67ac5a9dd9deb4333bf26))


### Tests

* **daemon:** add unit tests for stream-json parser in worker ([df754cb](https://github.com/abusi/clhorde/commit/df754cb59d313a9990ee7f3b3f09976ccb354537))
* **web:** add JS unit tests for frontend pure functions and state logic ([cd141ae](https://github.com/abusi/clhorde/commit/cd141aebe8523c680ddb5d11a09b8aef63219f9a))
* **web:** add Rust unit tests for HTTP bridge modules ([71ca450](https://github.com/abusi/clhorde/commit/71ca450958c52d601558216b338c699816a2395f))

## [0.3.0](https://github.com/abusi/clhorde/compare/clhorde-v0.2.0...clhorde-v0.3.0) (2026-03-25)


### Features

* **ci:** build and upload release binaries for Linux and macOS ([1fd4902](https://github.com/abusi/clhorde/commit/1fd49029c1f0f90874a64200eb85426311471b40))


### Bug Fixes

* **ci:** include all conventional commit types in release-please ([0aa3f38](https://github.com/abusi/clhorde/commit/0aa3f388116cae5f7a536069cfb6c6062a463412))
* **ci:** use simple release type for cargo workspace compatibility ([6c3cc9b](https://github.com/abusi/clhorde/commit/6c3cc9b78d371ca5e3d7360310e86f3aedcbd04d))


### Documentation

* add conventional commits convention to CLAUDE.md ([c8fb36c](https://github.com/abusi/clhorde/commit/c8fb36cd46283743148d4fe2232c336fbe19ef25))
