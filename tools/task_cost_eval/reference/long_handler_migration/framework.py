"""Tiny request router: handlers decorated with route() return a Response."""
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


def route(name, methods=('GET',)):
    def decorate(handler):
        if name in _ROUTES:
            raise ValueError(f'route {name} registered twice')
        _ROUTES[name] = (handler, tuple(methods))
        return handler
    return decorate


def routes():
    return {name: list(methods) for name, (_, methods) in sorted(_ROUTES.items())}


def handlers_in(module):
    return {name: handler for name, (handler, _) in _ROUTES.items() if handler.__module__ == module}


def dispatch(name, method, request):
    handler, methods = _ROUTES[name]
    if method not in methods:
        return Response(405, {'error': 'method not allowed'})
    result = handler(request)
    if not isinstance(result, Response):
        raise TypeError(f'{name} returned {type(result).__name__}, not Response')
    return result
