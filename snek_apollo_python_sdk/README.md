# SNEK_Apollo Python SDK

Integrate the high-performance SNEK_Apollo Multimedia Framework into your Python applications.

## Quick Start

1. **Build the Core**:
   Run `cargo build --release` in the root directory to generate `snek_apollo.dll`.
   
2. **Copy Files**:
   Copy `snek_apollo.dll` and `snek_apollo.py` into your project.

3. **Implementation**:

```python
from snek_apollo import SnekApolloPlayer

# Initialize
player = SnekApolloPlayer()

# Open a stream (HLS, MP4, MKV, etc.)
# If you provide an HWND, the player uses Hardware Accelerated Direct2D rendering.
info = player.open("https://example.com/video.m3u8", hwnd=None)

# Control
player.play()
player.pause()
player.seek(5000) # Seek to 5 seconds

# Get Position
pos_ms = player.get_position_ms()

# Cleanup
player.terminate()
```

## Advanced Usage: Hardware Rendering

SNEK_Apollo can render directly to any Win32 HWND (PyQt5, PySide6, Tkinter, etc.) using Direct2D.

```python
# PyQt5 Example
hwnd = int(self.video_label.winId())
player.open(url, hwnd=hwnd)
```

## GUI Example

See `snek_apollo_gui.py` for a full implementation of a modern video player using PyQt5 and SNEK_Apollo.
