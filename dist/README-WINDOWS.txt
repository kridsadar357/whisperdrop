WhisperDrop (Windows) — send & receive
======================================

SETUP
1. Run WhisperDrop-Setup-0.2.0-x64.exe when provided, or put
   WhisperDrop.exe in its own folder.
2. Double-click it. First run opens the browser-based setup wizard.
   Choose the receive folder, tunnel settings, edge position and firewall.
3. When Tunnel is enabled, enter the same strong pairing passphrase on every
   trusted device. Tunnel file contents are then end-to-end encrypted.

EVERYDAY USE
- RECEIVE: files land in  C:\Users\<you>\Downloads\BridgeReceived
          with the liquid-glass animation at the position you picked.
- SEND (drag & drop): drag files from Explorer onto the amber strip
          on the screen edge you chose -> pick the destination device
          from the popup menu -> progress shows in the console.
- SEND: drag files to the edge strip, then choose a LAN or tunnel device.
        Multiple files are queued one at a time.
- The app remains in the notification area. Right-click its icon for
  Preferences, received files, activity log, or Quit.

Re-open Preferences from the notification-area icon, or run:
WhisperDrop.exe setup
Preview the transfer animation:  WhisperDrop.exe --demo-overlay
Command line:  WhisperDrop.exe send <file> --to <device|ip|group>   |   WhisperDrop.exe devices   |   WhisperDrop.exe status

Cross-network (tunnel) needs the relay service running on
riki-api.online — see the relay/ folder in the project.
