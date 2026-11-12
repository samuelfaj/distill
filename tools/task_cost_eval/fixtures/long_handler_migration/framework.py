"""Tiny request router.

register() is the old API: its handlers return a dict with status, body and
optional headers, or a (status, body) tuple. route() and Response are the
new API: handlers decorated with route() return a Response.
"""
_ROUTES = {}


class Response:
    def __init__(self, status, body, headers=None):
        self.status = status
        self.body = body
        self.headers = dict(headers or {})

    def __eq__(self, other):
        return isinstance(other, Response) and (self.status, self.body, self.headers) == (
            other.status, other.body, other.headers)

    def __repr__(self):
        return f'Response({self.status!r}, {self.body!r}, {self.headers!r})'


def _add(name, handler, methods, legacy):
    if name in _ROUTES:
        raise ValueError(f'route {name} registered twice')
    _ROUTES[name] = (handler, tuple(methods), legacy)


def register(name, handler, methods=('GET',)):
    _add(name, handler, methods, legacy=True)


def route(name, methods=('GET',)):
    def decorate(handler):
        _add(name, handler, methods, legacy=False)
        return handler
    return decorate


def _from_legacy(result):
    if isinstance(result, tuple):
        status, body = result
        return Response(status, body)
    return Response(result['status'], result['body'], result.get('headers'))


def routes():
    return {name: list(methods) for name, (_, methods, _) in sorted(_ROUTES.items())}


def handlers_in(module):
    return {name: handler for name, (handler, _, _) in _ROUTES.items() if handler.__module__ == module}


def dispatch(name, method, request):
    handler, methods, legacy = _ROUTES[name]
    if method not in methods:
        return Response(405, {'error': 'method not allowed'})
    result = handler(request)
    if legacy:
        return _from_legacy(result)
    if not isinstance(result, Response):
        raise TypeError(f'{name} returned {type(result).__name__}, not Response')
    return result
