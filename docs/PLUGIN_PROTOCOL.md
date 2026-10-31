# Server extension protocol (anoncat plugins)

Whisker finds out at runtime what a homeserver adds through hot-loaded WASM
plugins, so new server features need no client release to show up. A server
that doesn't implement this API answers 404, and Whisker treats that as "no
plugins". Plain Synapse, Conduit and other servers keep working.

All endpoints use the normal Matrix access token (`Authorization: Bearer …`).

## List plugins

`GET /_anoncat/client/v1/plugins`

```json
{
  "generation": 7,
  "plugins": [
    {
      "id": "org.anoncat.ephemeral",
      "version": "0.2.0",
      "description": "Rooms whose history the server deletes after a timer",
      "capabilities": ["ephemeral_rooms"],
      "methods": ["create", "set_ttl"]
    }
  ]
}
```

- `id`: reverse-DNS, unique on the server.
- `capabilities`: feature flags. Whisker shows or hides UI by these, never by plugin id, so two
  plugins can provide the same capability.
- `methods`: what `call` accepts. Whisker refuses to call anything not listed.
- `generation`: increase it every time a plugin is loaded, reloaded or unloaded. A client
  that sees a new number fetches the list again.

## Call a method

`POST /_anoncat/client/v1/plugins/{id}/call/{method}`, with a JSON body in and JSON out.
Use normal Matrix error bodies (`{"errcode": "...", "error": "..."}`) with a 4xx/5xx status.

## Telling clients about a reload

Put the current `generation` in the `unstable_features` of `/_matrix/client/versions` as
`"org.anoncat.plugins.gen.<n>": true`, or send it in the `account_data` event
`org.anoncat.plugins` (`{"generation": n}`). (Whisker doesn't watch for this yet; for now it
runs discovery when asked.) In the client this is `whisker_core::anoncat::ServerExtensions::{discover, has, call}`.

## Rules for plugin authors

- Plugins run on the server, so privacy promises are the server's to keep. Say in
  `description` what a plugin stores.
- Never require a plugin for basic chat. A Whisker user on another server must still be
  able to talk to people on anoncat.
