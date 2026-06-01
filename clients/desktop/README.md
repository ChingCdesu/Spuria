# Spuria Desktop Client (Tauri)

A GUI front-end over the `spuria-client` library — both **controller (主控)** and
**host (被控)** roles, no CLI required. The Rust backend ([`src-tauri/src/main.rs`](src-tauri/src/main.rs))
drives the same `app::run` state machine as the CLI and forwards
`ClientEvent`s to the web frontend ([`ui/`](ui/)).

The UI has two pages:

- **Connect** — per-connection inputs (role, peer id, local listener/RDP target,
  force-relay) and live status/log.
- **Settings** — the **global** signaling server, reflect (srflx) address and
  team secret. These are saved to the local computer in `settings.json` under
  the OS app-data dir (via the `save_settings`/`get_settings` backend commands)
  and shared across all connections — not re-entered per connect.

## Prerequisites

- Rust (stable) and Node.js 18+.
- Windows: the **WebView2 runtime** (preinstalled on Windows 11; the MSI also
  bootstraps it).

## Develop

```sh
cd clients/desktop
npm install
npm run dev        # tauri dev — hot window, live logs
```

## Build the MSI

```sh
cd clients/desktop
npm run build      # -> src-tauri/target/release/bundle/msi/Spuria_<ver>_x64_en-US.msi
```

The frontend is static (no bundler): `tauri.conf.json` points `frontendDist` at
[`ui/`](ui/) and `withGlobalTauri` exposes `window.__TAURI__`.

> `createUpdaterArtifacts` is on, so a release build also signs the updater
> bundle and needs the `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]` env vars (below).
> For a quick **unsigned** local MSI, build just that target:
> `npm run build -- --bundles msi`. CI signs the full set on tagged releases.

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
