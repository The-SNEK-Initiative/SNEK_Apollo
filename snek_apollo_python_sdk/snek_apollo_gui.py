import sys
import time
import threading

from PyQt5.QtCore import Qt, QTimer
from PyQt5.QtGui import QImage, QPixmap
from PyQt5.QtWidgets import (
    QApplication,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QPushButton,
    QSlider,
    QStyle,
    QVBoxLayout,
    QWidget,
)

from snek_apollo import SnekApolloPlayer, PLAYER_STATE_PLAYING


class SnekApolloGUI(QMainWindow):
    def __init__(self, url):
        super().__init__()
        self.setWindowTitle("SNEK_Apollo - Multimedia Framework")
        self.resize(1024, 768)

        self.player = SnekApolloPlayer()
        self.last_frame_bytes = None
        self.url = url
        self.is_hls = ".m3u8" in url.lower()
        self.is_live = False
        self.duration_ms = 0
        self.is_muted = False
        self.volume = 1.0
        self.pending_seek_ms = None
        self.seek_in_progress = False
        self.last_seek_time = 0
        self.accumulated_target_ms = 0

        self.init_ui()
        self.hwnd = int(self.video_label.winId()) if sys.platform == "win32" else 0

        self.open_thread = threading.Thread(target=self.bg_open_stream, daemon=True)
        self.open_thread.start()

        self.loading_timer = QTimer()
        self.loading_timer.timeout.connect(self.update_loading_progress)
        self.loading_timer.start(50)

        self.timer = QTimer()
        self.timer.timeout.connect(self.update_frame)

        self.seek_timer = QTimer()
        self.seek_timer.setSingleShot(True)
        self.seek_timer.timeout.connect(self.commit_accumulated_seek)

    def init_ui(self):
        central_widget = QWidget()
        self.setCentralWidget(central_widget)
        layout = QVBoxLayout(central_widget)

        self.setFocusPolicy(Qt.StrongFocus)

        self.video_label = QLabel("Initializing Video...")
        self.video_label.setAlignment(Qt.AlignCenter)
        self.video_label.setStyleSheet("background-color: black;")
        self.video_label.setAttribute(Qt.WA_OpaquePaintEvent)
        layout.addWidget(self.video_label, 1)

        slider_layout = QHBoxLayout()
        self.timeline = QSlider(Qt.Horizontal)
        self.timeline.setFocusPolicy(Qt.NoFocus)
        self.time_label = QLabel("00:00 / 00:00")
        self.timeline.sliderPressed.connect(self.begin_seek)
        self.timeline.sliderMoved.connect(self.preview_seek)
        self.timeline.sliderReleased.connect(self.commit_seek)
        slider_layout.addWidget(self.timeline)
        slider_layout.addWidget(self.time_label)
        layout.addLayout(slider_layout)

        controls_layout = QHBoxLayout()

        self.btn_play = QPushButton()
        self.btn_play.setIcon(self.style().standardIcon(QStyle.SP_MediaPause))
        self.btn_play.setFocusPolicy(Qt.NoFocus)
        self.btn_play.clicked.connect(self.toggle_play)

        self.btn_stop = QPushButton()
        self.btn_stop.setIcon(self.style().standardIcon(QStyle.SP_MediaStop))
        self.btn_stop.setFocusPolicy(Qt.NoFocus)
        self.btn_stop.clicked.connect(self.stop_video)

        self.btn_mute = QPushButton()
        self.btn_mute.setIcon(self.style().standardIcon(QStyle.SP_MediaVolume))
        self.btn_mute.setFocusPolicy(Qt.NoFocus)
        self.btn_mute.clicked.connect(self.toggle_mute)

        self.volume_slider = QSlider(Qt.Horizontal)
        self.volume_slider.setRange(0, 100)
        self.volume_slider.setValue(100)
        self.volume_slider.setFixedWidth(150)
        self.volume_slider.setFocusPolicy(Qt.NoFocus)
        self.volume_slider.valueChanged.connect(self.change_volume)

        controls_layout.addWidget(self.btn_play)
        controls_layout.addWidget(self.btn_stop)
        controls_layout.addStretch(1)
        controls_layout.addWidget(self.btn_mute)
        controls_layout.addWidget(self.volume_slider)

        layout.addLayout(controls_layout)

    def bg_open_stream(self):
        try:
            self.info = self.player.open(self.url, self.hwnd)
            QTimer.singleShot(0, self.on_stream_opened)
        except Exception as e:
            print(f"Error opening stream: {e}")
            QTimer.singleShot(0, lambda: self.video_label.setText(f"Error: {e}"))

    def update_loading_progress(self):
        progress = self.player.get_download_progress()
        if progress > 0.0:
            pct = int(progress * 100)
            if pct >= 100:
                self.video_label.setText("Finalizing HLS Video (Custom Remuxing)...")
            else:
                self.video_label.setText(f"Prefetching HLS Video... {pct}%")

    def on_stream_opened(self):
        self.loading_timer.stop()
        self.is_live = (self.info['duration_ms'] == 0)
        self.setup_stream(self.info['width'], self.info['height'], self.info['duration_ms'])
        self.timer.start(16)
        self.player.play()

    def setup_stream(self, width=0, height=0, duration_ms=0):
        self.duration_ms = duration_ms

        if width > 0 and height > 0:
            screen = QApplication.primaryScreen().availableGeometry()
            max_w = int(screen.width() * 0.8)
            max_h = int(screen.height() * 0.8)
            scale = min(max_w / width, max_h / height, 1.0)
            self.resize(int(width * scale), int(height * scale) + 100)
            self.move(screen.center() - self.rect().center())

        if self.is_live:
            self.time_label.setText("\u25cf LIVE")
            self.time_label.setStyleSheet("color: #ff4444; font-weight: bold;")
            self.timeline.setEnabled(False)
            self.timeline.setRange(0, 1)
        else:
            self.time_label.setStyleSheet("")
            self.timeline.setEnabled(True)
            self.timeline.setRange(0, min(int(self.duration_ms), 2_147_483_647))
            self.time_label.setText(f"00:00 / {self.format_time(self.duration_ms)}")

    def format_time(self, ms):
        s = max(0, int(ms)) // 1000
        m = s // 60
        h = m // 60
        if h > 0:
            return f"{h:02}:{m % 60:02}:{s % 60:02}"
        return f"{m:02}:{s % 60:02}"

    def update_frame(self):
        if self.seek_in_progress:
            return

        pos_ms = self.player.get_position_ms()
        if not self.is_live:
            clamped = min(int(pos_ms), 2_147_483_647)
            if not self.timeline.isSliderDown():
                self.timeline.setValue(clamped)
                self.time_label.setText(f"{self.format_time(pos_ms)} / {self.format_time(self.duration_ms)}")

        frame_data = self.player.next_frame()
        if frame_data:
            width, height, data, _pts = frame_data
            self.last_frame_bytes = data
            image = QImage(self.last_frame_bytes, width, height, QImage.Format_RGB32).copy()
            pixmap = QPixmap.fromImage(image)
            self.video_label.setPixmap(
                pixmap.scaled(self.video_label.size(), Qt.KeepAspectRatio, Qt.SmoothTransformation)
            )

    def toggle_play(self):
        if self.seek_in_progress:
            return

        if self.player.get_state() == PLAYER_STATE_PLAYING:
            self.player.pause()
            self.btn_play.setIcon(self.style().standardIcon(QStyle.SP_MediaPlay))
        else:
            self.player.play()
            self.btn_play.setIcon(self.style().standardIcon(QStyle.SP_MediaPause))

    def stop_video(self):
        self.player.stop()
        self.close()

    def begin_seek(self):
        if self.is_live:
            return
        self.pending_seek_ms = self.timeline.value()
        self.preview_seek(self.pending_seek_ms)

    def preview_seek(self, ms):
        if self.is_live:
            return
        target_ms = self.clamp_seek_ms(ms)
        self.pending_seek_ms = target_ms
        self.time_label.setText(f"{self.format_time(target_ms)} / {self.format_time(self.duration_ms)}")

    def commit_seek(self):
        if self.pending_seek_ms is None:
            return
        self.seek_video(self.pending_seek_ms)
        self.pending_seek_ms = None

    def clamp_seek_ms(self, ms):
        max_ms = int(self.duration_ms) if not self.is_live else int(ms)
        return max(0, min(int(ms), max_ms))

    def seek_video(self, ms):
        if self.is_live:
            return
        clamped = self.clamp_seek_ms(ms)
        self.timeline.setValue(min(clamped, 2_147_483_647))
        self.time_label.setText(f"{self.format_time(clamped)} / {self.format_time(self.duration_ms)}")
        self.player.seek(clamped)
        self.seek_in_progress = False

    def change_volume(self, value):
        if self.seek_in_progress:
            return
        self.volume = value / 100.0
        self.player.set_volume(self.volume)

    def toggle_mute(self):
        if self.seek_in_progress:
            return
        self.is_muted = not self.is_muted
        self.player.set_mute(self.is_muted)
        if self.is_muted:
            self.btn_mute.setIcon(self.style().standardIcon(QStyle.SP_MediaVolumeMuted))
        else:
            self.btn_mute.setIcon(self.style().standardIcon(QStyle.SP_MediaVolume))

    def accumulate_seek(self, delta_ms):
        now = time.time()
        if now - self.last_seek_time > 1.0:
            self.accumulated_target_ms = self.player.get_position_ms()

        self.accumulated_target_ms = self.clamp_seek_ms(self.accumulated_target_ms + delta_ms)
        self.last_seek_time = now
        self.timeline.setValue(min(int(self.accumulated_target_ms), 2_147_483_647))
        self.time_label.setText(f"{self.format_time(self.accumulated_target_ms)} / {self.format_time(self.duration_ms)}")
        self.seek_timer.start(200)

    def commit_accumulated_seek(self):
        self.seek_video(int(self.accumulated_target_ms))

    def keyPressEvent(self, event):
        if event.isAutoRepeat():
            event.ignore()
            return

        if self.seek_in_progress and event.key() != Qt.Key_Escape:
            event.ignore()
            return

        if event.key() == Qt.Key_Space:
            self.toggle_play()
        elif event.key() == Qt.Key_Right and not self.is_live:
            self.accumulate_seek(10000)
        elif event.key() == Qt.Key_Left and not self.is_live:
            self.accumulate_seek(-10000)
        elif event.key() == Qt.Key_Escape:
            self.stop_video()
        elif event.key() == Qt.Key_Up:
            self.volume_slider.setValue(min(100, self.volume_slider.value() + 10))
        elif event.key() == Qt.Key_Down:
            self.volume_slider.setValue(max(0, self.volume_slider.value() - 10))

    def closeEvent(self, event):
        self.player.cleanup()
        event.accept()


if __name__ == "__main__":
    app = QApplication(sys.argv)

    url = "https://samplelib.com/mp4/sample-10s.mp4"
    if len(sys.argv) > 1:
        url = sys.argv[1]

    gui = SnekApolloGUI(url)
    gui.show()
    sys.exit(app.exec_())
