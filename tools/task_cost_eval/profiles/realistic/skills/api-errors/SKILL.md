---
name: api-errors
description: Shape error responses of the HTTP API consistently.
---
# Api errors

Errors are JSON objects with `code` (stable snake_case), `message` (human readable) and optional `details`. Never leak stack traces. Validation errors use status 422 and list each failing field.
