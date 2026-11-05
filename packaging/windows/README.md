# Windows packaging contract

The production binary is `templartorrent.exe`, not the fixture preview. Registration
must be per-user and explicit, without requiring administrator privileges.

Register the magnet URL protocol under HKCU\Software\Classes, plus a private ProgID
for .torrent. The command must quote the installed executable path and the activation
argument; the Rust application parses it as one argument, never evaluates it as a
shell command. Support Unicode installation/destination paths and long-path policy
where supported. Use owner-restricted named-pipe single-instance forwarding with a
bounded payload, command acknowledgment and foreground activation.

## Implemented (2026-11-05)

- `templartorrent.exe --register` writes the per-user entries in
  `integration/templartorrent/src/winreg.rs`: the `TemplarTorrent.Magnet` and
  `TemplarTorrent.Torrent` ProgIDs with `"<exe>" "%1"`, an `OpenWithProgids` entry
  for `.torrent`, `Capabilities` plus `RegisteredApplications` so the app appears
  under Settings > Apps > Default apps, and the `magnet` protocol class itself only
  when no other program owns it. When another program is the default, Settings opens
  at this app's page: Windows protects the user's choice and no program may set it.
  `--unregister` removes those entries, and the `magnet` class only if its command
  still runs a `templartorrent.exe`. Register again after moving the executable.
  Writes are per value, not transactional.
- The release binary is a GUI-subsystem program, so a browser or Explorer launch
  does not show a console. A terminal launch attaches to its console. With no
  console, a launch error is shown in a message box.
- Single-instance forwarding uses loopback TCP plus a 16-byte random token in
  `%LOCALAPPDATA%\templartorrent\run\instance.port`, not a named pipe. The token
  file inherits the per-user ACL; the payload is bounded and acknowledged. The
  forwarder calls `AllowSetForegroundWindow` and the running window restores and
  raises itself.
- Clipboard: Ctrl+V uses liminalkit-shell's Windows backend (arboard /
  clipboard-win), on liminalkit branch `windows-clipboard`.

No registry script is supplied that would point at a nonexistent production binary.
Implement association install/uninstall transactionally in the packaging stage;
uninstall only associations still owned by this application. Windows clipboard,
DPI, file sharing, deletion after handle closure and crash-recovery tests are release
gates. Provide a portable archive as well as an optional per-user installer.
