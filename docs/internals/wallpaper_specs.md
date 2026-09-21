# Wallpaper Backend Specifications

## GNOME (Wayland & X11)
- Tool: GSettings / dconf over D-Bus
- Schema: `org.gnome.desktop.background`
- Keys:
  - Light mode: `picture-uri` (value format: `'file:///path/to/img.jpg'`)
  - Dark mode: `picture-uri-dark` (value format: `'file:///path/to/img.jpg'`)
- Style: `picture-options` ('none', 'wallpaper', 'centered', 'scaled', 'stretched', 'zoom', 'spanned')

## KDE Plasma (Wayland & X11)
- Interface: `org.kde.plasmashell`
- Path: `/PlasmaShell`
- Method: `org.kde.PlasmaShell.evaluateScript`
- Script template:
  ```javascript
  let allDesktops = desktops();
  for (let i = 0; i < allDesktops.length; i++) {
      let d = allDesktops[i];
      d.wallpaperPlugin = "org.kde.image";
      d.currentConfigGroup = Array("Wallpaper", "org.kde.image", "General");
      d.writeConfig("Image", "file:///path/to/img.jpg");
  }
  ```

## Hyprland (Wayland)
- Protocol: IPC via Unix Domain Socket (`$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`)
- Supported tools: `hyprpaper` (send `preload /path`, `wallpaper monitor,/path`) or `swww`.

## Windows (Win32)
- API: `SystemParametersInfoW`
- Action: `SPI_SETDESKWALLPAPER` (0x0014)
- Flags: `SPIF_UPDATEINIFILE | SPIF_SENDCHANGE`