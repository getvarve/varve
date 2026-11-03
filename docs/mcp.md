# Local MCP server

Varve can serve the [Model Context Protocol](https://modelcontextprotocol.io) on your own computer, so AI tools such as Claude Code can read and change your folders, connection profiles, saved queries and settings. It is off by default.

## Turn it on

Open **Settings > Local MCP server** and switch on **Serve MCP on this computer**. Varve then listens on `http://127.0.0.1:7465/mcp` (pick another port on the same card) for as long as the app runs. The card shows the exact commands below with the paths of your installation, ready to copy.

## Add it to Claude Code

Two ways, both need Varve to be running with the server on.

**stdio (recommended).** Claude Code starts Varve's bridge, which reads the token by itself:

```bash
# macOS
claude mcp add --scope user varve -- /Applications/Varve.app/Contents/MacOS/varve mcp
# Linux (AppImage install)
claude mcp add --scope user varve -- ~/.local/bin/varve mcp
```

On macOS use `~/Applications/Varve.app/...` when the installer put Varve there. On Windows copy the command from the settings card: it names the installed `varve.exe`.

**Streamable HTTP.** Claude Code connects to the server and sends the token as a bearer token:

```bash
claude mcp add --scope user --transport http varve http://127.0.0.1:7465/mcp \
  --header "Authorization: Bearer $(cat ~/Library/Application\ Support/Varve/mcp-token)"
```

The token file is in Varve's data folder: `~/Library/Application Support/Varve` on macOS, `~/.local/share/Varve` on Linux, `%APPDATA%\Varve` on Windows (or `VARVE_DATA_DIR`). With HTTP, Claude Code stores the token in its own settings, so after **New token…** you add the server again. The stdio bridge reads the new token by itself.

Check it with `claude mcp list`, or ask Claude for "my Varve connections".

Other MCP clients work the same way: start `varve mcp` as a stdio server, or connect to the URL with the `Authorization` header.

## Tools

| Tool | What it does |
| --- | --- |
| `get_workspace` | Every folder, connection profile and saved query |
| `get_folder`, `get_connection`, `get_saved_query` | One record (a folder with counts of what it holds) |
| `create_folder`, `update_folder`, `move_folder`, `delete_folder` | Folders; deleting either moves the contents up or deletes them too |
| `create_connection`, `update_connection`, `move_connection`, `duplicate_connection`, `delete_connection` | Connection profiles |
| `create_saved_query`, `update_saved_query`, `delete_saved_query` | Saved queries (stored, never run) |
| `reorder` | The manual order of sibling folders, connections or saved queries |
| `get_settings`, `update_settings` | Activity log text level and retention, remembering secrets for the session, automatic update checks |
| `forget_cached_secrets` | Forgets the secrets Varve keeps in memory |

Updates take `changes`, merged into the current fields like a JSON merge patch: nested objects (`tls`, `ssh`, `options`) merge, and `null` clears a field that can be empty (such as `database`, or `port` for the engine default). `null` for a field without an empty value (`environment`, `options.readOnly`) is refused, as is an unknown field, with the list of known ones. The window shows every change at once; the settings page keeps edits you have not saved and says the stored values changed.

## Safety

- **Passwords never pass through MCP.** No tool reads or saves a password or passphrase. A tool changes a connection through the same code as the edit form, so when the host, port, user or route changes, Varve removes the saved passwords that no longer apply, exactly as when you edit it yourself.
- **Only programs that can read the token file get in.** Every request needs `Authorization: Bearer <token>`. The token is 32 random bytes in `mcp-token`, readable only by you (mode 0600 on macOS and Linux). **New token…** replaces it at once.
- **The bridge checks who answers before it sends the token.** When Varve was killed, its endpoint file can stay behind, and another program (another user's, on a shared computer) may listen on the port by now. So `varve mcp` first asks the server to prove it holds the token (an HMAC of a random value and the server's own address, at `/mcp/identity`) and sends the token only to a server that can. The address in the proof stops a relay on another port, such as a local debugging proxy, from passing Varve's proof on. The bridge also ignores `HTTP_PROXY` and `ALL_PROXY` and follows no redirects. An HTTP client sends its header without such a check, so on a computer you share with other users prefer the stdio bridge.
- **Web pages are refused.** The server listens on `127.0.0.1` only and, on every path, refuses any `Host` other than `localhost` or `127.0.0.1` (DNS rebinding) and every request with an `Origin` header, which browsers send and MCP clients do not.
- **Destructive tools say so.** The delete tools and `update_settings` (a shorter retention deletes activity events) carry the MCP `destructiveHint`, and the server asks clients to confirm with you before changing or deleting a production connection.
- **A broken call is not repeated.** When the connection to Varve breaks during a tool call, the bridge reports it instead of sending the call again, since the change may already have been made. A call the server refused before running it (an old token after **New token…**) is sent once more on a new connection.
- **One server per data folder.** A second Varve window with the same data folder does not start its own server on a taken port: it says another Varve window serves MCP there, and leaves that window's endpoint file alone.

## Why the server is inside the app

A change to a connection is more than a row in `app.db`: Varve takes its record locks, moves saved secrets out of the keychain when a profile points at another server, and queues keychain removals it retries later. A separate program writing `app.db` would skip all of that. So the server runs in the app process and calls the same services as the window, and `varve mcp` only forwards stdio to it. The code is in `app/src-tauri/src/mcp`; the tests are in `app/src-tauri/tests/mcp.rs`.
