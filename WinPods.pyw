"""Double-click launcher: starts WinPods without a console window."""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from winpods.cli import main_gui  # noqa: E402

sys.exit(main_gui())
