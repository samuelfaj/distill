import json
from pathlib import Path


def load_sales(path):
    return json.loads(Path(path).read_text())
