"""Allow `python -m winpods`."""

import sys

from .cli import main

sys.exit(main())
