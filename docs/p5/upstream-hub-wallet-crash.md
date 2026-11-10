# Arkiv Hub: Connect wallet crashes the page (draft report for Arkiv)

Status: DRAFT, not sent.
Found 2026-11-10 while claiming testnet tGLM at https://hub.arkiv.network/faucet.

## Symptom

On the faucet page, Connect wallet opens the RainbowKit dialog.
Choosing any option that shows a QR code (WalletConnect, or a wallet whose extension is not installed) replaces the whole page with "This page couldn't load".
The console shows `Uncaught Error: invalid border=0`, thrown inside a `useMemo` in the QR component.
It happens in every browser and at every window size, because it is a dependency bug, not a rendering quirk.

## Root cause

- `@rainbow-me/rainbowkit@2.2.11` (the latest release, 2026-05-06) pins `cuer` to exactly `0.0.3`.
- `cuer@0.0.3` builds the WalletConnect QR with `encodeQR(value, "raw", { border: 0, ... })` and declares `qr: "~0"`.
- `qr@0.6.0` (2026-04-28) started rejecting `border <= 0`; `qr@0.5.5` only checked that the border was an integer.
- So any install made after April resolves `qr@0.6.0` or newer (0.7.2 today), and the first QR render throws, which unmounts the React tree.

Upstream: wevm/cuer issue #12, fixed by PR #11 and released as `cuer@0.0.4` on 2026-10-18; RainbowKit has not picked it up yet.
RainbowKit fix opened 2026-11-10: https://github.com/rainbow-me/rainbowkit/pull/2716 (bumps `cuer` to 0.0.4; fixes rainbow-me/rainbowkit#2677).
Once it is released, the hub can drop the override below and upgrade RainbowKit instead.

## Fix (one line)

Force `cuer@0.0.4` under RainbowKit.

npm (`package.json`):

```json
"overrides": { "cuer": "0.0.4" }
```

pnpm: `"pnpm": { "overrides": { "cuer": "0.0.4" } }`; yarn: `"resolutions": { "cuer": "0.0.4" }`.
Then reinstall, rebuild and deploy.
Drop the override once RainbowKit ships a release that depends on `cuer@0.0.4` or newer.

## Verification

Clean install of `@rainbow-me/rainbowkit@2.2.11`, then the same QR call RainbowKit makes:

| Install | Resolved | Result |
|---|---|---|
| as published | `cuer@0.0.3` + `qr@0.7.2` | `invalid border=0` (the production error) |
| with the override | `cuer@0.0.4` + `qr@0.7.2` | 29x29 grid |
| old working pair | `cuer@0.0.3` + `qr@0.5.5` | 29x29 grid, byte-identical to the fixed one |

The fixed QR is the same code as before the regression, so wallets scan it unchanged.

## Workaround until it is deployed

Use a browser with a wallet extension installed (MetaMask, Rabby, Coinbase Wallet) and pick that wallet in the dialog.
An injected wallet connects without rendering a QR code, so the crash path is never reached.
