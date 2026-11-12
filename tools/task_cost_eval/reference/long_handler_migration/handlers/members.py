from framework import Response, route

UNIT = 4


def _ok(body):
    return Response(200, body)


@route('members.get')
@route('members.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'members', 'total': request.get('qty', 1) * UNIT}
    return _ok(body)
