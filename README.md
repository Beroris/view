<img src="assets/icon.png" width="64" height="64" alt="">

# View

A fast viewer for Windows: photos first, and videos and music too. Double-click a photo and it's there, sharp, before you've let go of the mouse. Arrow keys flip through the folder with no wait, because the next pictures are already decoded. Select a file in File Explorer and press Space to see it, like Quick Look on a Mac. Nothing phones home, and the whole thing is one small exe.

## What it does

- **Opens instantly.** A 12-megapixel photo is on screen about 80 ms after you open it. View decodes a screen-sized copy first, straight from the JPEG's own data, then the full resolution only if you zoom in past it.
- **Flips through folders.** The images either side of the current one are decoded ahead of time, in the direction you're going. Sort by name (with numbers in order, so `IMG 2` comes before `IMG 10`), name letter by letter, date modified, date created or size. It wraps at the ends, and new or deleted files show up when you come back to the window.
- **Zoom that follows the mouse.** The wheel zooms around the cursor and glides between steps; a touchpad pinch follows your fingers. Drag to pan. Middle-click, or click the percentage, switches between fit and 100%. Past 300% you see real pixels instead of a blur, which you can turn off. Hold Z to look at 100% for as long as the key is down. Turn on "Keep zoom between photos" (Options) and the zoom and place stay put as you flip, for comparing shots.
- **Every common format.** JPEG, PNG, GIF, WebP, BMP, TIFF, ICO, plus HEIC, AVIF, JPEG XL and camera RAW when Windows has the codec (see below). Animated GIF, WebP and APNG play, with their own timing and loop counts.
- **Right colours.** Photos with a colour profile (Adobe RGB, Display P3 and so on) are converted to sRGB, so they don't look washed out or oversaturated. EXIF orientation is followed, so phone photos stand the right way up.
- **Transparency shown properly.** On a checkerboard, or on the background, white or black, as you prefer.
- **Rotate and flip**, for viewing, and Ctrl+S saves the turn into a JPEG or TIFF without re-encoding it: View only changes the photo's orientation tag, so not one pixel is touched.
- **Copy and share.** Copy the image (as a PNG and a bitmap, so it pastes anywhere), the file itself, or its path. Open the folder with the file selected, open it with another app, or see its Windows properties.
- **Delete to the Recycle Bin**, with a confirmation you can turn off. It moves on to the next picture, and an Undo button (or Ctrl+Z) puts the file straight back.
- **Filmstrip and grid.** A strip of thumbnails above the toolbar shows where you are in the folder; click one to jump to it, or scroll over it to flip through. Press G for every file in the folder as a grid of thumbnails: arrow keys or the mouse to pick, Enter to open, + and − (or Ctrl and the wheel) for bigger or smaller thumbnails.
- **Edit a copy.** Press E to crop (free, or to the original shape, square, 4:3, 3:2 or 16:9), turn, straighten, and change brightness, contrast, saturation and warmth. Save a copy writes `Photo (edited).jpg` beside the original, which is never changed.
- **Drag a photo out** into an email, a chat or another folder: hold Ctrl and drag, or just drag when the whole picture fits on screen in full screen. **Set as desktop background** is in the menu.
- **Information panel.** Size, format, colour space, bit depth, frames, file dates, and the camera's details: camera, lens, exposure, aperture, ISO, focal length, flash, white balance and location. Drag across any of it, or double-click a word, to select it and copy it with Ctrl+C. The same goes for messages, dialogs and the Space preview's file cards.
- **Videos and music.** Videos and songs sit in the folder with your pictures and play right there, through Windows' own Media Foundation, so whatever Windows can play, View can. Click to play or pause, drag along the bar to seek, and double-click for full screen. Point at the speaker button and a volume slider slides out beside it. Songs show their cover, title, artist and album. Play faster or slower (0.25× to 2×) from the speed button or with > and <. Step a paused video a frame at a time with , and ., and Ctrl+C copies the frame on screen. Go to another file and back and a video carries on where you left it; turn on "Remember video positions after closing" (Options) and it does after you close View too, which suits films.
- **The picture gets the window.** No title bar: the name and window buttons float over the top of the photo, the tools sit in a frosted-glass bar at the bottom, and the arrows only appear when the mouse nears a side. All of it fades when the mouse rests, and Tab hides it altogether. One picture fades into the next. Light, dark or Windows' own theme, in your Windows accent colour.
- **Space to preview, in File Explorer.** Turn it on in the installer or in View's menu (Options), then select any file in Explorer or on the desktop and press Space. Pictures open in a window sized to them, videos at a size you can watch, and songs play with their cover. PDFs show their pages, Markdown is laid out like a page, code and JSON are coloured, and CSV files show as a table. PowerPoint slides show as they look, with their layouts, theme colours and fonts, pictures, tables and shapes; Word and Excel files show their text and tables; zip files show their folders, which open with a click, fonts show their letters at several sizes, and folders show how many files they hold, how big they are, and a few of their pictures. Pages and text glide when you scroll. Select text by dragging, in PDFs too (double-click picks a word, and holding the mouse past the edge carries on down the document), then copy it with Ctrl+C or the Copy button. Ctrl+F finds text (F3 or Enter for the next match; the box takes clicks, drags, double-clicks, Shift with the arrows, Ctrl+A, cut and paste like any other), and Ctrl+G, or clicking the page number, goes to a page. Scanned PDFs with no text in them are read with Windows' text recognition, so you can find and select their text too. Each document opens where you left it. 3D models (STL, 3MF and OBJ) show in 3D with their size in millimetres: drag to turn them, scroll to zoom, and double-click to set them straight again. Other files show their thumbnail, type, size and date. Space or Esc closes it, and the arrow keys move to the next file with the preview following. It closes when you click away, like a pop-up; or, picked in Options, it stays open on top of everything, or stays open like any other window that comes back up with Explorer. Kept open, it can be dragged by its top edge. It's a tiny helper that starts when you sign in (about 2 MB of memory, no CPU while you're not pressing keys), and it's off unless you turn it on.
- **Big and broken files are safe.** A 384-megapixel PNG opens in under half a second at screen size. Files that are cut short show what's there; files that aren't images, or say they're impossibly large, get a plain message instead of a crash.

## Formats

| Format | Opens |
|---|---|
| JPEG, PNG, GIF, BMP, TIFF, ICO, JPEG XR, DDS | Always, using Windows' own codecs |
| WebP, animated WebP, APNG, animated GIF | Always, using View's own decoders |
| HEIC / HEIF | With "HEIF Image Extensions" and "HEVC Video Extensions" from the Microsoft Store |
| AVIF | With "AV1 Video Extension" from the Microsoft Store |
| JPEG XL | With "JPEG XL Image Extension" from the Microsoft Store |
| Camera RAW (CR2, CR3, NEF, ARW, DNG and others) | With "Raw Image Extension" from the Microsoft Store |
| Video: MP4, MOV, MKV, WebM, AVI, WMV and others | Always for H.264, VP9 and the common ones, using Windows' Media Foundation. HEVC and AV1 need their Microsoft Store extensions |
| Music: MP3, M4A, AAC, FLAC, WAV, Ogg, Opus, WMA | Always, using Windows' Media Foundation |

If a codec is missing, View says which extension to install instead of failing quietly. **About View** in the menu lists what this PC can open.

## Keys

| Key | What it does |
|---|---|
| Right, Left, Space, Backspace, Page Down, Page Up | Next and previous file |
| Space or K (video or song) | Play and pause |
| J / L, Shift+Left / Shift+Right | Back or forward 10 seconds / 5 seconds |
| M | Mute (point at the speaker button for the volume slider, or turn the wheel over it) |
| Home / End | First / last image in the folder |
| Up / Down | Pan when zoomed in, otherwise previous / next |
| Ctrl+Left / Ctrl+Right | Pan sideways |
| Mouse wheel | Zoom around the cursor (or next and previous: right-click → Options) |
| + / − | Zoom in / out |
| Ctrl+0 / Ctrl+1 | Fit to window / actual size |
| Middle-click | Switch between fit and 100% |
| F11, F, Alt+Enter, double-click | Full screen |
| Esc | Leave full screen, then close the information panel (and close View, if you turn that on) |
| R / Shift+R | Rotate right / left |
| H / V | Flip horizontally / vertically |
| I | Information panel |
| Ctrl+C | Copy the image, the video frame on screen, or the selected text |
| Ctrl+Alt+C / Ctrl+Shift+C | Copy the file / its path |
| Delete | Move to the Recycle Bin |
| Ctrl+Z | Put back the last file deleted |
| G | All files in the folder as a grid |
| E | Edit a copy: crop, straighten, light and colour |
| > / < (video) | Faster / slower |
| Ctrl+O | Open a file |
| Ctrl+E | Show in its folder |
| Ctrl+Enter | Windows properties |
| F5 | Reload |
| F1 | About View, and which formats this PC can open |
| Tab | Hide all of View's controls |
| Mouse back / forward buttons | Previous / next image |
| Right-click, Menu key | Everything else: sorting, appearance and options |

## Install

Download **View-&lt;version&gt;-Setup.exe** from [Releases](https://github.com/Beroris/view/releases/latest) and run it. It installs for you only, so it doesn't ask for admin rights. It adds View to the Start menu and to "Open with" for images. To make View the default, tick the last option in the installer, or go to Settings → Apps → Default apps → View. Uninstall it from Settings → Apps like any other app.

To run View without installing, download **View-&lt;version&gt;-Portable.exe** instead. Run it from anywhere, a USB stick included. It keeps its settings in a "View settings" folder beside itself rather than in your profile.

## Resource use

Measured on the release build on Windows 11, with a 2560×1440 monitor and a Radeon RX 6800 XT.

| | |
|---|---|
| Download | 2.5 MB single exe, no runtime needed |
| Open a 12 MP JPEG, from double-click to picture on screen | 80 ms |
| Open a 48 MP JPEG | 190 ms |
| Next image in a folder of 1600×1000 JPEGs | ready before you press the key; 9 ms each to prefetch |
| Memory with a small image open | 39 MB private |
| Memory after flipping through 150 photos | 136 MB private; only images near the current one are kept |
| CPU while showing an image | 0 ms over 10 s |
| Space preview helper, waiting | 1.8 MB private |

## Build

You need Rust 1.85 or newer.

```sh
cargo build --release -p view-windows        # target/release/view.exe
cargo test --workspace
pwsh scripts/package-windows.ps1             # dist/: the installer and the portable exe (needs Inno Setup 6)
```

Other tools:

```sh
cargo run --release -p view-core --example fixtures -- <dir>   # test images: orientation, colour profiles, animation, damaged files…
cargo run --release -p view-core --example pdftext -- <pdfs>   # the text View finds in PDFs, page by page, with timings
view.exe --decode-bench <files>                                # decode timings, written to %TEMP%\view-bench.txt
set VIEW_TRACE=1                                               # startup and decode timings in %TEMP%\view-trace.txt
node scripts/gen_icon.mjs                                      # redraw the icon
```

## Layout

| Path | What |
|---|---|
| `crates/view-core` | Platform-neutral: folder scanning and natural sorting, zoom and pan, the decoded-image cache, EXIF orientation and metadata, colour profile names, settings, the GIF, WebP and APNG decoders, the STL, 3MF and OBJ readers, the PDF text reader (with decryption for PDFs that open without a password), the zip, Word and Excel readers, PowerPoint slides laid out with their layouts, masters and themes, and the text, Markdown and CSV parsing behind the previews, and the photo edits (crop, straighten, light and colour). No OS calls. |
| `platforms/windows` | The Windows app: Win32, Direct2D on Direct3D 11, WIC for decoding and colour management, DirectWrite, Media Foundation for video and music, Windows' PDF renderer, and Direct3D for 3D models. |
| `installer` | The setup wizard (Inno Setup), built by `scripts/package-windows.ps1`. |
| `assets/fonts` | Geist, the interface font, built into the exe (SIL Open Font License; see `Geist-LICENSE.txt`). |

## Privacy

View never connects to the internet. There's no account, no telemetry and no update check. Settings are in `%APPDATA%\View\settings.json`.

## License

GPL-3.0. See [LICENSE](LICENSE). The Geist font in `assets/fonts` keeps its own SIL Open Font License.
