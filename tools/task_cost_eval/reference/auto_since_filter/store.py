import json
from pathlib import Path


def load(path):
    return json.loads(Path(path).read_text())
