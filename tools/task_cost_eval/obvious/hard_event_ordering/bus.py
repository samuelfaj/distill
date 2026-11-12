class EventBus:
    """Synchronous publish/subscribe."""

    def __init__(self):
        self._handlers = {}

    def subscribe(self, topic, handler, once=False):
        """Register handler(payload) for topic; returns a function that unsubscribes it."""
        entry = (handler, once)
        handlers = self._handlers.setdefault(topic, [])
        handlers.append(entry)

        def unsubscribe():
            if entry in handlers:
                handlers.remove(entry)
        return unsubscribe

    def emit(self, topic, payload=None):
        errors = []
        handlers = self._handlers.get(topic, [])
        for entry in list(handlers):
            handler, once = entry
            if once and entry in handlers:
                handlers.remove(entry)
            try:
                handler(payload)
            except Exception as error:
                errors.append(error)
        if errors:
            raise errors[0]
