from framework import Response, route

UNIT = 34


@route('schedules.get')
@route('schedules.fetch')
def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'schedules', 'total': request.get('qty', 1) * UNIT}
    return Response(200, body)
