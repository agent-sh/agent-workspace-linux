#!/usr/bin/env python3
"""Cold XWayland Qt text, caret and selection fixture."""
import json
import sys
from pathlib import Path
from PyQt6.QtCore import QTimer
from PyQt6.QtWidgets import QApplication, QLineEdit, QVBoxLayout, QWidget
output = Path(sys.argv[1])
app = QApplication([])
app.setApplicationName('GnomeLegacyQtFixture')
window = QWidget()
window.setWindowTitle('Legacy Qt text fixture')
layout = QVBoxLayout(window)
entry = QLineEdit()
layout.addWidget(entry)
def save():
    output.write_text(json.dumps({'text': entry.text(), 'caret': entry.cursorPosition()}))
entry.textChanged.connect(save)
entry.cursorPositionChanged.connect(save)
window.resize(480, 200)
window.show()
entry.setFocus()
QTimer.singleShot(200, save)
app.exec()
