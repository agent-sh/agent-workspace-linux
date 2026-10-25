#!/usr/bin/env python3
"""Native Qt client recording actual clicks, text, scroll and compositor scaling."""
import json
from pathlib import Path
import sys
from PyQt6.QtCore import QTimer
from PyQt6.QtGui import QKeySequence, QShortcut
from PyQt6.QtWidgets import QApplication, QMainWindow, QPushButton, QPlainTextEdit

receipt = Path(sys.argv[1])
app = QApplication(sys.argv)
app.setApplicationName("WorkspaceCosmicFixture")
window = QMainWindow()
window.setWindowTitle("COSMIC native acceptance fixture")
window.resize(480, 360)
state = {"clicks": 0, "wheel_events": 0, "pointer": None}

class Button(QPushButton):
    def mousePressEvent(self, event):
        state["pointer"] = [event.position().x(), event.position().y()]
        super().mousePressEvent(event)

class Editor(QPlainTextEdit):
    def wheelEvent(self, event):
        state["wheel_events"] += 1
        super().wheelEvent(event)

editor = Editor(window)
editor.setGeometry(30, 40, 300, 110)
# A distinct color lets the test locate the editor from the native capture.
editor.setStyleSheet("background:#00ffff;color:black")
button = Button("Target", window)
button.setGeometry(240, 180, 60, 40)
button.setStyleSheet("background:#ff00ff;color:white;border:1px solid #ff00ff")

def clicked():
    state["clicks"] += 1

button.clicked.connect(clicked)
shortcut = QShortcut(QKeySequence("Ctrl+L"), window)
shortcut.activated.connect(editor.setFocus)

def save():
    state.update(text=editor.toPlainText(), selection=editor.textCursor().selectedText(), client_buffer_scale=window.devicePixelRatioF(),
                 screen_logical=[app.primaryScreen().geometry().width(), app.primaryScreen().geometry().height()])
    staged=receipt.with_suffix(".staging")
    staged.write_text(json.dumps(state))
    staged.replace(receipt)

timer=QTimer()
timer.timeout.connect(save)
timer.start(50)
window.show()
app.exec()
