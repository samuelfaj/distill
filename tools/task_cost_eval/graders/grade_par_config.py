#!/usr/bin/env python3
import configparser
import json
import sys

from _par_common import finish, fixture_dir


def main(argv):
    root = fixture_dir(argv, "par_config")
    failures = []
    try:
        app = json.loads((root / "config/app.json").read_text())
        if app != {"name": "demo", "port": 8081, "log_level": "debug"}:
            failures.append(f"app.json wrong: {app}")
    except ValueError as error:
        failures.append(f"app.json invalid: {error}")
    db = configparser.ConfigParser()
    try:
        db.read(root / "config/db.ini")
        if dict(db["database"]) != {"host": "localhost", "timeout": "30"}:
            failures.append(f"db.ini wrong: {dict(db['database'])}")
    except (configparser.Error, KeyError) as error:
        failures.append(f"db.ini invalid: {error}")
    log = (root / "docs/CHANGELOG.md").read_text()
    new, old = log.find("## 1.4.0"), log.find("## 1.3.0")
    if not 0 <= new < old:
        failures.append("CHANGELOG needs a '## 1.4.0' section above '## 1.3.0'")
    else:
        section = log[new:old]
        for text in ("database timeout to 30 seconds", "app to port 8081"):
            if text not in section:
                failures.append(f"CHANGELOG 1.4.0 section lacks {text!r}")
    if "Added health endpoint." not in log[old:]:
        failures.append("CHANGELOG lost the 1.3.0 entry")
    return finish(failures)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
