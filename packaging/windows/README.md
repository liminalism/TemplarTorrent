# Windows packaging contract

The production binary is `templartorrent.exe`, not the fixture preview. Registration
must be per-user and explicit, without requiring administrator privileges.

Register the magnet URL protocol under HKCU\Software\Classes, plus a private ProgID
for .torrent. The command must quote the installed executable path and the activation
argument; the Rust application parses it as one argument, never evaluates it as a
shell command. Support Unicode installation/destination paths and long-path policy
where supported. Use owner-restricted named-pipe single-instance forwarding with a
bounded payload, command acknowledgment and foreground activation.

No registry script is supplied that would point at a nonexistent production binary.
Implement association install/uninstall transactionally in the packaging stage;
uninstall only associations still owned by this application. Windows clipboard,
DPI, file sharing, deletion after handle closure and crash-recovery tests are release
gates. Provide a portable archive as well as an optional per-user installer.
