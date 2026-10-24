#!/usr/bin/env python3
"""Small native Qt target that records actual pointer delivery."""
import json
from pathlib import Path
import sys
from PyQt6.QtCore import QTimer
from PyQt6.QtWidgets import QApplication, QMainWindow, QPushButton, QLabel

output = Path(sys.argv[1])
state = {"count": 0, "pointer": None}
app = QApplication(sys.argv)
app.setApplicationName("WorkspaceScaleFixture")
state["screen_device_pixel_ratio"] = app.primaryScreen().devicePixelRatio()
state["screen_geometry"] = [app.primaryScreen().geometry().width(), app.primaryScreen().geometry().height()]
output.write_text(json.dumps(state))
window = QMainWindow()
window.setWindowTitle("Workspace Qt scale fixture")
window.resize(480, 360)

class Button(QPushButton):
    def mousePressEvent(self, event):
        state["pointer"] = {"x":event.position().x(), "y":event.position().y()}
        super().mousePressEvent(event)

button = Button("Target", window)
button.setGeometry(240, 160, 60, 40)
button.setAccessibleName("Target")
button.setStyleSheet("QPushButton { background-color: #ff00ff; color: white; border: 1px solid #ff00ff; }")
label = QLabel("Waiting", window)
label.setGeometry(30, 40, 300, 40)

def clicked():
    state["count"] += 1
    label.setText(f"Clicks: {state['count']}")
    save()

def save():
    state["device_pixel_ratio"] = window.devicePixelRatioF()
    state["screen_geometry"] = [app.primaryScreen().geometry().width(),app.primaryScreen().geometry().height()]
    staging=output.with_suffix(".staging")
    staging.write_text(json.dumps(state))
    staging.replace(output)

timer=QTimer()
timer.timeout.connect(save)
timer.start(250)
button.clicked.connect(clicked)
window.show()
app.exec()
