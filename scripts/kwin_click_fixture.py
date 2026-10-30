#!/usr/bin/env python3
"""Record native Qt button events and actual widget activation."""
import json
from pathlib import Path
import sys
from PyQt6.QtCore import QEvent, QObject, QTimer, Qt
from PyQt6.QtWidgets import QApplication, QPushButton, QWidget

output = Path(sys.argv[1])
app = QApplication(sys.argv)
window = QWidget()
window.setWindowFlags(Qt.WindowType.FramelessWindowHint)
window.setWindowTitle('KWin click regression')
window.resize(400, 220)
state = {'events': [], 'activations': 0, 'targets': {}, 'ticks': 0, 'scrolls': 0}
buttons = []

def save():
    for button in buttons:
        point = button.mapTo(window, button.rect().center())
        state['targets'][button.text()] = [point.x(), point.y()]
    staging = output.with_suffix('.staging')
    staging.write_text(json.dumps(state))
    staging.replace(output)

class Recorder(QObject):
    def eventFilter(self, target, event):
        if event.type() == QEvent.Type.Wheel:
            state['scrolls'] += 1
            save()
        if event.type() in (QEvent.Type.MouseButtonPress, QEvent.Type.MouseButtonDblClick,
                            QEvent.Type.MouseButtonRelease):
            state['events'].append({'target': target.text(), 'button': event.button().value,
                                    'kind': 'release' if event.type() == QEvent.Type.MouseButtonRelease else 'press'})
            save()
        return False

recorder = Recorder()
def activated():
    state['activations'] += 1
    save()

for index, name in enumerate(['left', 'right']):
    button = QPushButton(name, window)
    button.setGeometry(30 + index * 200, 60, 140, 80)
    button.installEventFilter(recorder)
    button.clicked.connect(activated)
    buttons.append(button)

timer = QTimer()
def tick():
    state['ticks'] += 1
    save()
timer.timeout.connect(tick)
timer.start(50)
window.show()
app.exec()
