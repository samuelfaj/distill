import importlib
import pkgutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_all():
    """Import every handler module once; each registers its routes at import time."""
    if str(HERE) not in sys.path:
        sys.path.insert(0, str(HERE))
    for module in pkgutil.iter_modules([str(HERE / 'handlers')]):
        importlib.import_module(f'handlers.{module.name}')
