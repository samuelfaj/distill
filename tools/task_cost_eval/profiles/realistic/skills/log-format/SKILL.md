---
name: log-format
description: Write log lines that the on-call rotation can search.
---
# Log format

Use key=value pairs after a short lowercase event name, for example `order_saved order_id=12 items=3`. No secrets, tokens or full payloads. Errors log once, where they are handled.
