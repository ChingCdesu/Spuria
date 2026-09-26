# Spuria Desktop Client (Tauri + React)

A GUI front-end over the `spuria-client` library — both **controller (主控)** and
**host (被控)** roles, no CLI required. The Rust backend ([`src-tauri/src/main.rs`](src-tauri/src/main.rs))
drives the same `app::run` state machine as the CLI and forwards `ClientEvent`s
to the frontend.

**Tech stack:** React + Vite + TypeScript + [Radix UI Themes](https://www.radix-ui.com/themes).
Source in [`src/`](src/); the layout is RustDesk-inspired.

The UI has two pages:

- **Home** — a *This Device* card (your ID + copy, local RDP target, "Allow
  remote control" → host role) and a *Control Remote Device* card (peer ID,
  local listener, force-relay → controller role), plus a live activity log.
- **Settings** — global, persisted config in sections: **Network** (signaling
  server, reflect address), **Security** (team secret), **Connection defaults**
  (listener/RDP target, force-relay, UDP multitransport), **Appearance** (theme),
  **Updates**, **About** (version, device id). Saved to the local computer in
  `settings.json` under the OS app-data dir via the `save_settings`/`get_settings`
  backend commands, shared across all connections.

## Prerequisites

- Rust (stable) and Node.js 18+.
- Windows: the **WebView2 runtime** (preinstalled on Windows 11; the MSI also
  bootstraps it).

## Develop

```sh
cd clients/desktop
npm install
npm run tauri dev    # Vite dev server + Tauri window, hot reload
# or just the web UI in a browser (no Tauri APIs): npm run dev
```

## Build the MSI

```sh
cd clients/desktop
npm run tauri build  # runs `npm run build` (tsc + vite) then bundles
#   -> src-tauri/target/release/bundle/msi/Spuria_<ver>_x64_en-US.msi
```

`tauri.conf.json` runs `npm run dev`/`npm run build` (Vite) via
`beforeDevCommand`/`beforeBuildCommand` and bundles `dist/`.

> `createUpdaterArtifacts` is on, so a release build also signs the updater
> bundle and needs the `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]` env vars (below).
> To compile the complete desktop executable without installers or signing:
> `npm run tauri -- build --no-bundle`. Merely selecting `--bundles msi` does
> not disable `createUpdaterArtifacts` or its signing requirement.

## Auto-update

Wired via `tauri-plugin-updater`. To enable it for real:

1. **Generate a signing keypair** (one time):
   ```sh
   npm exec tauri signer generate -- -w spuria-updater.key
   ```
   Keep `spuria-updater.key` (private) secret; copy the printed **public key**.
2. **Paste the public key** into `src-tauri/tauri.conf.json` → `plugins.updater.pubkey`.
3. **Set the update endpoint** in the same block. With GitHub Releases:
   ```
   https://github.com/<owner>/<repo>/releases/latest/download/latest.json
   ```
4. **Add CI secrets** to the repo:
   - `TAURI_SIGNING_PRIVATE_KEY` — contents of `spuria-updater.key`
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — its password (empty if none)
5. **Release**: push a tag `vX.Y.Z`. The [`release-client`](../../.github/workflows/release-client.yml)
   workflow builds the MSI, signs the updater bundle, generates `latest.json`,
   and attaches everything to a draft GitHub release. Publish it and clients
   running an older version will offer the update (the in-app **⟳ Updates**
   button calls the `check_update` command).

> Until a real `pubkey`/endpoint are configured, the app runs fine but
> "Check for updates" returns an error — that is expected.
