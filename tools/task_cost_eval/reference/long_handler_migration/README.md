Request handlers for the internal API. `app.load_all()` imports every module in handlers/,
and `framework.dispatch(name, method, request)` answers a request.

Check one handler module with `python3 check.py handlers/NAME.py`.
