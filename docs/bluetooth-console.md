# Native Linux Bluetooth settings

Settings → Controller → Bluetooth opens the console's device manager when a
BlueZ adapter is available. It uses the same menus, keyboard, controller input
and pointer handling as the rest of Punktfunk. A USB keyboard/controller is
needed to pair the first wireless controller.

The page can power the adapter, search for 60 seconds, pair/connect devices,
disconnect them and forget saved devices. Forget asks for confirmation. Device
rows show connection status and battery level when BlueZ supplies it. Pairing
supports PIN entry, passkey entry/display and explicit numeric confirmation.
Closing the page stops its discovery session and cancels pending pairing.

## Linux integration

The session launcher starts `pf_client_core::bluetooth`. Its worker reads BlueZ's
ObjectManager and calls Adapter1/Device1 over the system D-Bus. A connection-local
Agent1 handles pairing only for the device selected by the user. It does not
replace the desktop's default pairing agent. Pairing replies bypass device work;
the UI remains responsive while BlueZ waits. Successful pairing marks the device
trusted so BlueZ can reconnect it later. Adapter policy chooses the first powered
adapter, or the first adapter when none is powered.

Run as the normal client user. The OS must provide `bluetoothd`, a working adapter
and D-Bus permission to call BlueZ. Debian/Raspberry Pi OS's BlueZ policy already
permits these client calls; no root launcher or Punktfunk-specific helper service
is needed. Audio devices additionally need PipeWire's Bluetooth SPA plugin.
BlueZ owns and persists bond keys; Punktfunk never copies them into its settings.

## Updating the fork

Keep the change separate from Raspberry Pi decode/presentation work. The main
implementation is in `pf-client-core/src/bluetooth{,.rs}` and
`pf-console-ui/src/screens/bluetooth.rs`. Integration is limited to the core module
export/dependency, console startup, screen dispatch and the settings row. Other
platforms receive an unavailable snapshot and hide the menu entry.

After rebasing, run the client-core and console UI tests with the desktop feature
set, then check physical discovery, cancel, confirmation/PIN entry, reconnect and
forget. Test pairing with both a controller and a keyboard; USB input remains
available when the wireless controller is disconnected. A headless smoke probe is
available as `cargo run -p pf-client-core --example bluetooth_probe
--no-default-features --features desktop -- --scan` on a BlueZ machine.

The UI and backend require no StreamOS or StreamShell code. Image packaging only
supplies standard Linux Bluetooth/audio services and the rebuilt Punktfunk bundle.
