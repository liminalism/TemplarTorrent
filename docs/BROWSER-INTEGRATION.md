# Catching download links from Firefox and Chrome

**Status: review and guidance, not implemented.** DESIGN.md section 11 keeps a
browser extension out of this repository. This document records what it would
take to have Firefox and Chrome (and the Chromium family: Edge, Brave, Vivaldi,
Chromium) hand ordinary file downloads to TemplarTorrent on Linux and Windows,
so the work can be sized and decided on. The reference client reviewed for the
HTTP feature ships such a bridge; its layout is noted where it informed this
review. Browser facts below are as of November 2026 and should be re-checked
against the vendor documentation before implementation.

## 1. What the browser offers

There is no OS-level hook for "an http download started". Protocol handlers
(`x-scheme-handler/magnet` on Linux, the registry's `URL Protocol` key on
Windows) catch a *scheme*, which is how magnets reach the client today; `http`
and `https` belong to the browser. Catching a file download therefore means a
browser extension, and the only sanctioned way for an extension to talk to a
desktop program is **native messaging**: the browser starts a *native host*
executable the extension names, and the two exchange JSON over the host's
standard input and output. Everything else (a localhost web server the
extension posts to, a clipboard watcher, a file drop folder) is either an
unauthenticated local network service, which DESIGN.md section 9 rules out, or
a guess.

So the shape is fixed: one WebExtension, one native host, one host manifest
per browser per user, and a message format.

## 2. Detecting a download in the extension

Two signals exist, and the robust bridge uses both.

**`downloads.onCreated`.** Fires when the browser has decided to download
(the user clicked a link the server answered with `Content-Disposition:
attachment`, chose Save Link As, or a page triggered a download). The handler
sees the final URL, the suggested file name, the MIME type, the referrer and
the size when known. To take the download over, the extension calls
`downloads.cancel(id)` then `downloads.erase({id})` and sends the URL to the
host. The browser has already made one request by then; the server sees a
cancelled first connection, which is what every download manager with a
browser bridge does. Firefox and Chrome both expose this API with the
`downloads` permission. The reference bridge works this way, with a short pause
and a settings check (captured extensions, excluded sites) before it cancels.

**`webRequest.onHeadersReceived`.** Fires per response and lets the extension
inspect `Content-Type`, `Content-Length` and `Content-Disposition` before the
browser renders or saves. It is the way to catch a download *before* the
browser's own download starts, and to filter by extension or size. Blocking
(cancelling) from this handler needs `webRequestBlocking`, which Chrome's
Manifest V3 grants only to policy-installed extensions; ordinary Chrome
extensions can observe here but must act through the `downloads` API. Firefox
still grants `webRequestBlocking` to Manifest V2 and V3 extensions. A bridge
that must behave the same in both browsers observes with `webRequest` and
acts with `downloads`.

Either way the extension needs `permissions: ["downloads", "nativeMessaging"]`
plus `webRequest` with host permissions for the sites it watches (`<all_urls>`
for a general download manager, which the stores review closely).

What the extension should hand over: the final URL, the referrer, the
suggested file name, the cookies for that URL when the user opted in (many
downloads are authenticated), and the user agent. TemplarTorrent's URL job
would then need a way to carry request headers (cookies, referer) that the
current `AddUrl` command does not have. That is the one engine change this
feature forces; see section 6.

## 3. The native host

The host is a small executable the browser launches with the extension's
origin as an argument (Chrome) or the manifest path and extension id
(Firefox). It reads length-prefixed JSON from stdin: a 32-bit little-endian
byte count, then that many bytes of UTF-8 JSON. Replies go to stdout the same
way. Chrome limits a message from the host to 1 MB and a message to the host
to 4 GB; Firefox has similar limits. The host must never write anything else
to stdout, or the framing breaks.

For TemplarTorrent the host does not need to speak to the engine directly.
`activate.rs` already implements single-instance activation: a second launch
with a magnet, `.torrent` path or URL forwards it over the owner-only Unix
socket to the running window, which opens the Add dialog. The host would
therefore be a thin program that validates the message, then runs
`templartorrent <URL>` (or connects to the activation socket itself, reusing
the same bounded protocol), and answers the extension with accepted or
refused. Running the main binary from the host has one requirement the
reference client learned the hard way: the host must only start a verified,
regular executable at a path it owns, never a path from the message or a
user-writable location it did not install, or the bridge becomes a way for a
page to run programs.

Three decisions follow:

- **One binary or two.** `templartorrent --native-host` could serve as the
  host itself, which avoids a second artifact but puts the GUI binary (and
  its font loading) on the browser's start path. A separate `templartorrent-
  bridge` binary of a few hundred lines is cleaner and starts faster. The
  reference client ships a separate host.
- **Headless start.** If the window is not running, the host must start it.
  The activation code already handles "no primary instance": the launch
  becomes the primary and opens the Add dialog. The host should not keep the
  window alive after the browser closes the host; DESIGN.md forbids a
  background service.
- **Consent.** Opening the Add dialog, not starting the transfer, is the right
  behaviour: the user still chooses the folder and confirms. A setting to skip
  the dialog for a configured folder would be a later, explicit opt-in.

## 4. Registering the host, per browser and OS

The browser finds the host through a JSON manifest:

```json
{
  "name": "org.liminalism.templartorrent",
  "description": "TemplarTorrent download bridge",
  "path": "/absolute/path/to/templartorrent-bridge",
  "type": "stdio",
  "allowed_origins": ["chrome-extension://<extension id>/"]
}
```

Firefox uses `allowed_extensions: ["<add-on id>"]` instead of
`allowed_origins`, so two manifest files are needed. The `name` must match the
name the extension passes to `runtime.connectNative` / `sendNativeMessage` and
may contain only lowercase letters, digits, dots and underscores.

Where the manifest goes, per user (no administrator rights, the DESIGN.md
rule):

| Browser | Linux | Windows |
|---|---|---|
| Firefox | `~/.mozilla/native-messaging-hosts/<name>.json` | Registry `HKCU\Software\Mozilla\NativeMessagingHosts\<name>`, default value = full path to the manifest file |
| Chrome | `~/.config/google-chrome/NativeMessagingHosts/<name>.json` | `HKCU\Software\Google\Chrome\NativeMessagingHosts\<name>` |
| Chromium | `~/.config/chromium/NativeMessagingHosts/<name>.json` | `HKCU\Software\Chromium\NativeMessagingHosts\<name>` |
| Edge | `~/.config/microsoft-edge/NativeMessagingHosts/<name>.json` | `HKCU\Software\Microsoft\Edge\NativeMessagingHosts\<name>` |
| Brave | `~/.config/BraveSoftware/Brave-Browser/NativeMessagingHosts/<name>.json` | `HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\<name>` |
| Flatpak / Snap browsers | the browser's sandboxed config directory (for example `~/.var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts/`), and the host must be reachable from inside the sandbox, which usually means a Flatpak portal or an exported `flatpak-spawn --host` shim | n/a |

On Windows the `path` in the manifest must point at an `.exe` or a `.bat`
(Chrome refuses other extensions), and the registry value must be the path to
the manifest, not the executable. The TemplarTorrent installer or a `templartorrent
--register-browser-bridge` settings action would write these files and keys
for the browsers it finds; `--unregister` removes them. Registration must be an
explicit action, as DESIGN.md section 9 requires for every association.

## 5. Installing the extension itself

Chrome: Developer mode plus Load unpacked works for development and for the
reference client's users; a store listing (Chrome Web Store, one-time fee,
review for `<all_urls>` and `nativeMessaging`) is what ordinary users expect.
Policy-installed extensions (`ExtensionInstallForcelist`) are for managed
machines only.

Firefox: a permanently installed add-on must be signed by Mozilla (AMO, either
listed or self-distributed unlisted signing, free). Unsigned add-ons load only
temporarily through `about:debugging` and vanish at restart, or permanently in
Developer Edition, Nightly and ESR with `xpinstall.signatures.required=false`.
The reference client ships an unsigned ZIP and documents the temporary load,
which its own README calls a testing package rather than a release.

Both browsers want a Manifest V3 extension now; Firefox still accepts V2.
A single code base with two manifests (the reference client does this) is the
usual arrangement.

## 6. What TemplarTorrent itself would need

1. **Request headers on a URL job.** `Command::AddUrl` carries a URL, a folder
   and an optional digest. Browser-captured downloads often need the referer
   and cookies; adding an optional bounded header list (referer, cookie, user
   agent; never arbitrary headers) to the record and the request builder in
   `http/transport.rs` is the one engine change. Cookies are credentials:
   they would have to be stored with the same care as tracker passkeys
   (record permissions, redaction from every label) and cleared when the job
   finishes.
2. **Activation of URLs.** Already done: `valid_activation` accepts `http://`
   and `https://` payloads and the Add dialog pre-fills them.
3. **A bridge executable** (section 3) and **registration actions** (section
   4), both small, both platform-specific in file locations only.
4. **A Windows activation path.** Linux activation is the Unix socket. The
   Windows named pipe from DESIGN.md section 9 is not implemented, and the
   bridge on Windows depends on it (or on the fallback of starting a new
   primary instance each time, which is worse).
5. **Tests.** The bridge's framing and validation are unit-testable without a
   browser (feed length-prefixed JSON on stdin, check the activation message).
   The browser end can be driven by Playwright with the unpacked extension
   loaded into Chromium, which the test environment already has; Firefox
   extension loading under Playwright needs the remote debugging protocol
   path and is the harder half.

## 7. Recommendation

The bridge is a separate deliverable of about: a 150-line WebExtension (one
background script, two manifests), a 200-line Rust host that reuses
`activate.rs`, an installer step that writes two manifests and, on Windows,
registry keys, and the header carriage in the URL job. None of it changes the
product boundary beyond what section 11 already did, but the extension and its
store presence are a second artifact with its own release process, which is
why it stays outside this repository until the user decides to carry it. If
it is carried, do the header support in the engine first, with fixtures, so
the extension work lands on a client that can actually complete the
downloads it is handed.
