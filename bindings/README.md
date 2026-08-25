# JavaScript bindings

npm packages generated from `credsync-ffi`.

| Package | Arrives at | Contents |
|---|---|---|
| `@credsync/react-native` | CS-27 | Turbo Module + TypeScript API (subscribe, status, dead-letter surface) |
| `@credsync/web` | later | `wasm-bindgen` build + OPFS storage adapter, when a PWA needs offline |

Empty until CS-27. The web adapter is deliberately deferred (`DECISIONS.md` O-005): it lands only
when the Dream Lab PWA needs offline, not before.
