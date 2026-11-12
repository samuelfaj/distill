from collections import deque


class EventBus:
    """Synchronous publish/subscribe.

    Events emitted while another is being delivered wait in a queue, so each event
    reaches all of its handlers before the next one starts. An event goes to the
    handlers subscribed when it was emitted; a once handler runs for one event only.
    """

    def __init__(self):
        self._handlers = {}
        self._queue = deque()
        self._delivering = False

    def subscribe(self, topic, handler, once=False):
        """Register handler(payload) for topic; returns a function that unsubscribes it."""
        entry = {'handler': handler, 'once': once, 'active': True}
        self._handlers.setdefault(topic, []).append(entry)

        def unsubscribe():
            if entry['active']:
                entry['active'] = False
                self._handlers[topic].remove(entry)
        return unsubscribe

    def emit(self, topic, payload=None):
        """Deliver an event; raises the first handler error once the queue is empty."""
        self._queue.append((topic, payload, list(self._handlers.get(topic, []))))
        if self._delivering:
            return
        self._delivering = True
        first_error = None
        try:
            while self._queue:
                current, value, entries = self._queue.popleft()
                for entry in entries:
                    if entry['once']:
                        if not entry['active']:
                            continue
                        entry['active'] = False
                        self._handlers[current].remove(entry)
                    try:
                        entry['handler'](value)
                    except Exception as error:
                        if first_error is None:
                            first_error = error
        finally:
            self._delivering = False
            self._queue.clear()
        if first_error is not None:
            raise first_error
