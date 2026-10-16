# rfm

[日本語](README.ja.md) · English

A lightweight, tabbed file manager for **Windows 10 and 11**, written in Rust. rfm uses native Windows controls and a virtual file list, without Electron, a WebView, or a separate GUI runtime.

rfm is for everyday work with filesystem folders. It is not a replacement for the Windows desktop or taskbar, and does not aim to reproduce every File Explorer feature.

## Build and run

Distribution is currently **source-build only**. Build on Windows with Rust **1.88 or newer** using the MSVC toolchain, Visual Studio Build Tools with **Desktop development with C++**, and the Windows SDK.

```powershell
git clone https://github.com/dikmri/rfm.git
cd rfm
cargo build --release
.\target\release\rfm.exe
```

You can also double-click `target\release\rfm.exe`. Pass folders as arguments to open them in separate tabs:

```powershell
.\target\release\rfm.exe "C:\Users" "C:\Windows"
```

Command-line folders take precedence over the configured startup location. rfm does not register itself as the default file manager or change Windows file associations.

For development, the repository also provides tests and can be checked with:

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
```

## Features

- Independent tabs with navigation history, selection, scroll position, filtering, sorting, and view mode. Returning to a parent folder restores its position and selects the child folder you came from.
- Details, list, small-icon, and medium-icon views; natural name sorting with folders first; optional extensions, hidden files, and selection checkboxes.
- A compact native menu bar, clickable breadcrumbs, editable full paths, back/forward history, and a resizable navigation pane. An optional compact command bar is available from the View menu.
- Windows file icons, file type names, and UI fonts, with DPI-aware layout. File sizes are rounded up to KB.
- Explorer-compatible copy/cut/paste, new folders, inline rename, Recycle Bin deletion, and confirmed permanent deletion.
- Windows `IFileOperation` for copying and moving: Windows handles conflicts, replacement, skipping, progress, and cancellation rather than silently overwriting files.
- Native shell context menus (`IContextMenu2/3`), default-app opening, properties, and path copying. Registered extensions such as Send to, Open with, and 7-Zip remain Windows-managed.
- Drag files out using Windows data objects; external drops are copied into the current folder after confirmation.
- Shell change notifications refresh ordinary folder views. Directory reads and file operations run on workers, and obsolete results are discarded.

The menus are rfm's own compact interface, not File Explorer's ribbon. The Share menu opens the Windows shell menu for the selection directly. Shell extensions are queried when their context menu is needed, not all enumerated at startup; their speed and UI also depend on the installed extensions.

## Settings and language

Open **File → Settings…**. Settings are saved to `%LOCALAPPDATA%\rfm\settings.ini`; no extra background service is required.

### Language

Under **Appearance → Language**, choose **Auto (Windows display language)**, **日本語**, or **English**. The choice takes effect **the next time rfm starts**; saving it does not change the current window's language.

Automatic uses Japanese when the Windows user UI language is Japanese, and English otherwise. Application-owned menus, dialogs, help, and messages support both languages. **Windows shell menus, shell extensions, OS-provided folder/type names, and OS error messages remain in the OS language.** File names and paths are never translated.

### Startup and window placement

Choose the home folder (default), the folder in the active tab when rfm last closed, or a specific absolute folder path. This is **not a restore-all-tabs option**. Startup changes apply on the next launch.

Window size, position, and maximized state are saved on normal exit. Exiting while minimized restores the pre-minimized state next time. Windows placement handling adjusts an off-screen saved position when, for example, a monitor has been disconnected.

### Icons

Choose **Windows default (Explorer style)** (default), **Lucide**, or **Tabler** under Appearance. Unlike language changes, icon changes apply immediately when saved and persist across restarts.

The Windows theme uses public theme APIs, actual known-folder icons, and Windows symbol fonts where necessary; it does not extract private Explorer images from DLLs. Lucide and Tabler replace navigation/search/command icons, tab and breadcrumb folders, and the six known-folder icons in the navigation pane. File-list icons, drives, and Quick Access entries with custom icons retain Windows system icons.

The application icon and the optional icon themes are embedded in the executable at multiple resolutions. No adjacent `.ico` file, external icon directory, or runtime SVG renderer is needed. Third-party icon licenses are listed [below](#license).

### Taskbar pinning

Keep the executable in its intended location before pinning it; do not move or delete it afterward. If a pin from an older build shows the wrong icon, unpin it, close the old build, run the updated executable, and pin its running taskbar icon again. rfm does not delete your pins or clear the system-wide icon cache.

## Quick Access

rfm **shares Windows Quick Access**, rather than maintaining a separate favorites file. Pin/unpin changes therefore also affect Windows' shared list.

- Use **File → Add to Quick Access** to add selected folders, or the current folder if no folder is selected.
- Use **File → Choose folders to add…** for the Windows multi-folder picker.
- Right-click navigation-pane entries to open, open in another tab, add, or unpin them. The File menu can also unpin the current folder.
- Adding is not a toggle that removes an existing pin. Unpinning never deletes the actual folder.
- Pinned and frequently used folders use the Windows ordering. Retrieval errors appear in the status area.
- Windows may take time to publish changes. rfm refreshes immediately and again after a delay; press `F5` after changes made in another app.

Names and paths appear before slow shell icons finish loading. Refreshes retain the existing list and cached icons while loading, and restore selection and scroll position by path and section. Enumeration and icon loading each use a single worker; old results cannot overwrite a newer list. A non-interruptible Windows icon call can still take time, without requiring the list to wait for all icons.

## Open in terminal

Use **File → Open in terminal**, or **Open in configured terminal** in a folder/background context menu, including navigation-pane folders.

With one folder selected, rfm opens that folder. With one file selected, it uses the file's parent. With no selection, it uses the current folder. Clear a multiple selection before using the File menu command.

The default terminal is **Alacritty**, which must be installed separately. Configure it in **File → Settings… → Terminal**:

- **Executable:** `alacritty.exe` by default. If it is not on `PATH`, browse to its installed executable. Do not include arguments or surrounding quotes in this field.
- **Arguments:** `--working-directory "{folder}"` by default. `{folder}` is replaced with the target path. For another terminal, use that application's working-directory argument syntax. Empty arguments can also be saved.

Changes apply to the next terminal launch without restarting rfm. The child process's working directory is also set to the target folder, but some terminals override it unless given their own argument.

The executable is started directly, without implicitly invoking `cmd /C` or another shell. Arguments are parsed using Windows quoting rules before substitution within each argument, keeping paths containing spaces or `&` within their original argument boundaries. `.bat`, `.cmd`, and `.lnk` are not accepted in the executable field. rfm does not install a terminal, silently fall back to another one, or change Windows' default terminal or existing shell extensions.

## Search and preview

- Typing in the search box filters names in the current folder immediately. Press `Enter` there for a case-insensitive substring filename search through subfolders.
- Press `Escape` in the file list to return from search results. Changing the query/location or closing a tab cancels the old search.
- Recursive search does not follow reparse points such as junctions. Hidden-folder traversal follows the hidden-files setting; inaccessible locations are counted.
- Toggle preview with `Alt+P` or the View menu. Text preview reads the first **256 KiB** and supports UTF-8, BOM-marked UTF-16, the Windows default code page, and LF/CRLF line endings.
- Images and other supported files use Windows thumbnail providers; unsupported formats show a reason. rfm does not continuously host arbitrary preview extensions.
- Preview work starts only when the pane is visible, coalesces selection changes on one worker, and ignores stale results.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl+T` / `Ctrl+W` | Add / close tab; closing the last tab exits |
| `Ctrl+N` | Open the current folder in a new window |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+1`–`Ctrl+8` / `Ctrl+9` | Tab by position / last tab |
| `Ctrl+L` / `Alt+D` | Edit the full address path |
| `Ctrl+F` | Focus search; type to filter, `Enter` to search recursively |
| `Alt+Left` / `Alt+Right` | Back / forward |
| `Alt+Up` / `Backspace` | Parent folder; Backspace applies when the list has focus |
| `Enter` / double-click | Open the selection |
| `Ctrl+Enter` | Open selected folders in separate tabs |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste |
| `Ctrl+Shift+C` | Copy selected paths, or the current folder if nothing is selected |
| `Ctrl+A` | Select all |
| `Space` | Toggle the focused item's selection when checkboxes are shown |
| `Ctrl+Shift+N` | New folder |
| `F2` | Inline rename; `Enter` confirms, `Escape` cancels |
| `Delete` | Move to Recycle Bin |
| `Shift+Delete` | Permanently delete, with No selected initially in the confirmation |
| `Alt+Enter` | Properties of the selection or current folder |
| `Alt+P` | Toggle preview |
| `Ctrl+H` | Toggle hidden files |
| `F5` | Refresh the list, Quick Access, and preview |
| `Escape` | Cancel rename/address editing, or leave search results |
| `F1` | Help |

While editing a name, address, or search query, copy/cut/paste/select-all/Backspace remain text-editing operations. Folder refreshes wait during inline rename rather than discarding your input. When extensions are shown, rename initially selects the base name; when hidden, the original extension is preserved.

Tabs can also be added with `+`, closed with `×` or middle-click, and scrolled with the arrows or mouse wheel when they overflow.

## Safety and limitations

- Multi-item copy, move, and delete are **not transactions**. Cancellation or errors can leave some items completed; check both source and destination afterward.
- Rename and new-folder operations check reserved and existing names. Permanent deletion processes reparse-point links themselves rather than following their targets.
- Recycle Bin operations use the Windows shell. Confirmation, progress, and media without Recycle Bin support follow Windows behavior. The older shell API used here has long-path limits.
- Search is **filename search**, not Windows Search indexing, AQS, or file-content search. Search results are not recursively watched; press `F5` to find changes deeper in the tree.
- During recursive search, renaming/deleting a folder being scanned may be temporarily restricted to guard against its replacement with a reparse point.
- Cancellation is checked around OS filesystem calls. An OS call already blocked on an unresponsive network cannot necessarily be interrupted immediately.
- The file list uses system icons, not a thumbnail grid. Preview formats depend on installed thumbnail providers.
- A common shell context menu is not supported for multiple items from different parent folders, such as recursive search results. Single-item menus and copy/move remain available.
- Explorer's ribbon, arbitrary shell virtual folders, Recycle Bin browsing, virtual browsing inside ZIP files, and reopening closed tabs are not supported.
- File operations request Windows Undo records, but rfm has no custom Undo/Redo history UI or shortcuts.

## Implementation notes

The file list is an owner-data Windows list view, not one control per file. Tabs and breadcrumbs use GDI; names, search keys, sort order, file types, and extension icons are cached. Directory I/O, search, and file operations run off the UI thread. Windows APIs are used at shell integration boundaries rather than replacing the whole file view with a shell browser.

The `windows-sys`, `windows`, and `windows-core` dependencies are Windows API/COM bindings, not external GUI runtimes. Release builds use optimization, LTO, a single codegen unit, and symbol stripping. Icons, their license notices, and source metadata are embedded at build time; resource compilation failure fails the build rather than producing an executable without its icons.

Japanese and English UI strings are compiled into the executable; no external language packs or runtime translation catalogs are required.

`examples/shell_probe.rs` and `benchmark-engines.ps1` are optional comparison tools for a separate Windows `IExplorerBrowser` application, not part of the normal app. Historical measurements on one Windows 10 x64 environment are not guarantees for the current version, cold starts, network folders, installed extensions, or system-wide resource use. The compared applications do not have identical features.

The comparison script uses a temporary settings profile, so its app windows do not replace your saved startup folder or window placement.

## License

rfm's application code is licensed under the [MIT License](LICENSE), copyright © 2026 dikmri.

Bundled third-party icons have their own notices, separate from the application's MIT license:

- [Lucide](https://lucide.dev/): ISC, with MIT for Feather-derived icons. See the full [Lucide license notices](assets/icons/lucide/LICENSE).
- [Tabler Icons](https://tabler.io/icons): MIT. See the full [Tabler license](assets/icons/tabler/LICENSE).
- [Icon source metadata](assets/icons/sources.json) records upstream URLs, pinned revisions, and rasterization details. Both license files and this metadata are also embedded in the executable.
