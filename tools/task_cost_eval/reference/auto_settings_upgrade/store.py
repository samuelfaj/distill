import json
from pathlib import Path

from schema import migrate


def load(path):
    return migrate(json.loads(Path(path).read_text()))


def save(path, settings):
    Path(path).write_text(json.dumps(settings, indent=2, sort_keys=True) + '\n')
