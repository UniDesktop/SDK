# System Appearance Specifications

## Linux (FreeDesktop Portal - Modern Standard)
- Bus: Session Bus
- Dest: `org.freedesktop.portal.Desktop`
- Path: `/org/freedesktop/portal/desktop`
- Interface: `org.freedesktop.portal.Settings`
- Method: `Read(namespace: "org.freedesktop.appearance", key: "color-scheme")`
  - 0: Default / Unknown
  - 1: Prefer Dark
  - 2: Prefer Light
- Signal: `SettingChanged(namespace, key, value)`

## Windows
- Registry: `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
- Key: `AppsUseLightTheme` (DWORD: 0 = Dark, 1 = Light)